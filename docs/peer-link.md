# QUIC link between paired servers

Paired Studio servers send their signed requests over one QUIC connection per server pair. The
HTTPS channel through Tailscale Serve stays as the fallback. The link changes only the
transport: request signatures, nonces, receipts, request IDs and the Tailscale owner check
stay as in the multi-server credential contract and `docs/cross-server-orchestration.md`.

## Measurement

Two Macs on one Wi-Fi network, a direct Tailscale path, 2026-10-14:

| Transfer                                 | HTTPS (Node helper and Serve) | QUIC link      |
| ---------------------------------------- | ----------------------------- | -------------- |
| One small request                        | about 600 ms                  | 10 ms (median) |
| 512 KiB JSON chunks, one after the other | about 2 MiB/s                 | 8.7 MiB/s      |
| One raw stream of 430 MiB                | not available                 | 23 MiB/s       |

The network limit is about 24 MiB/s (`scp` of 200 MiB). The HTTPS time is mostly the Node
process that each request starts, and the Serve hop.

## Process

`studio-peer-link` (`workspaces/runtime/apps/peer-link`, Rust, `quinn` and `rustls`) is a child
of the backend (`codex_peer_link.py`). The backend writes its configuration as the first stdin
line, so the private key is never in the arguments or the environment:

```json
{
  "listen": "100.x.y.z:4621",
  "privateKey": "<PKCS#8 PEM>",
  "publicKey": "<SPKI PEM>",
  "peers": ["<SPKI PEM>"],
  "backendPort": 4620,
  "forwardedHost": "<Serve host>",
  "socket": "<state>/multi-server/peer-link.sock"
}
```

The process answers `{"ready": true, "port": N}`. A later line `{"peers": [...]}` replaces the
paired keys and closes the connections of a removed key. The process exits when stdin closes,
so it ends with its backend. The backend starts it when Serve is on, checks it every 30 seconds,
and starts it again after an exit. It listens on the Tailscale IPv4 address of the Mac, on UDP
port 4621, or on a free port when 4621 is in use.

## Identity

TLS 1.3 with raw public keys (RFC 7250), ALPN `studio-peer/1`. Each side presents its Ed25519
pairing key:

- the client accepts only the key of the paired server that it dials;
- the server accepts only a client key of a paired server;
- the server refuses a source address outside the Tailscale ranges before the handshake.

The receiving backend then checks each request as a Serve request (below), including the
request signature and the Tailscale whois of the source address.

## Streams

Each request is one bidirectional stream: a 4-byte length and a JSON head
`{method, path, headers, bodyLength}`, then the body. The answer is a 4-byte length and a JSON
head `{status, headers}`, then the body until the stream ends. Limits: a head of 64 KiB, 64
headers, a request body of 64 MiB, the methods GET, POST, PUT, PATCH and DELETE, and paths
under `/api/`.

The receiving link sends the request to the backend on `127.0.0.1` with the headers that Serve
sets: `Host`, `X-Forwarded-Host` (the Serve host), `X-Forwarded-Proto: https` and
`X-Forwarded-For` (the QUIC source address). It removes these headers, `Tailscale-*`,
`Forwarded` and the hop headers from the peer's request. The backend boundary
(`studio_api/multi_server/boundary.py`) is the same for both transports.

## Backend socket

The backend sends one request per connection to `peer-link.sock`: a JSON line
`{address, publicKey, method, path, headers, bodyLength, timeout}`, then the body. The answer
is a JSON line `{status, headers}`, then body parts (a 4-byte length and data) and a zero length
at the end. A failure is a JSON line `{failure, sent}`.

## Route and fallback

A signed response to a paired server carries `X-Studio-Link: ADDRESS:PORT` when the link runs.
The client keeps the latest address per server and uses the link for the next requests.
Orchestration and multi-server access management use the same QUIC-first signed-request
exchange and share this learned-route state.

- `sent: false`: the paired server cannot have run the request (no connection, a refused key,
  a failed send). The client sends the same signed request over HTTPS and does not try the
  link of that server for 60 seconds.
- `sent: true` without an answer: the outcome is unknown. The client reports
  `remote_unavailable` with "Keep the request ID", as for a lost HTTPS response. It never
  sends the request again by itself.

A TLS 1.3 client can send before the server checks the client key. A connection that the
server closes with a TLS error ran no request, so it counts as `sent: false`.

## Files

`GET /api/servers/exports/{name}?offset=N` returns a file of `server-exports/` (an app archive
`<uuid>.zip` or a Git bundle `<uuid>.bundle`) as raw bytes from the offset. Only a paired
server can read it, and only over the link (the route is not in the OpenAPI schema). The update
puller (`docs/peer-updates.md`) receives the archive as one stream. When the stream is not
available or stops, the `update.chunk` actions continue from the received offset. The whole
archive checksum is checked as before.

## Not yet

- Git bundle fetch and the chat move still use their chunk actions.
- Events (command output, agent events, reachability) still use requests.
- The window still reaches a paired server over HTTPS (WebTransport later).

## Checks

- `cargo test -p studio-peer-link`
- `python3 -m unittest test_codex_peer_link` in `workspaces/runtime/apps/server/src` after
  `cargo build --release -p studio-peer-link` (two link processes, fake backends: the Serve
  headers, a 3 MiB answer, the answer limit, a refused key, a revoked key)
- `python3 -m unittest test_codex_peer_updates` (the stream and its continuation by chunks)
