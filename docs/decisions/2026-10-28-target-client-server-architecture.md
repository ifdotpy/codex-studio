# Target client and server architecture

- **Status:** Proposed, 2026-10-28. The owner accepted the client model (one frontend, thin cache). The other parts wait for review.
- **Audience:** maintainers who plan backend, client, and multi-server work.

## In short

One server process on each machine owns all state and all business logic. Every UI (the
desktop window, a browser, the phone) is one frontend that renders server data and sends
commands. The frontend keeps only a thin cache and a buffer of unsent input. Every user
action is one command with a stable id in one table.

## Context

Facts measured on 2026-10-28 at `85663a40a`:

- About 10 receipt tables, 4 outboxes, and 2 signature schemes (`studio-multi-server-v1`,
  `studio-federation-v1`) do the same job for different features.
- A user message has about 6 state vocabularies with about 11 statuses, in the browser,
  SQLite, the account worker, and the Claude bridge.
- Stop takes 5 to 6 hops to the provider, has no request id, pauses the agent, and
  cancels the whole queue. A queued message cannot be withdrawn after `reserved`.
- `client-core` (Rust, about 19.9k lines) does not depend on any runtime crate. The v1
  signature and the paired transport exist in both. On a machine with a server, the
  client keeps a second SQLite copy of the server projections.
- The web client writes every stream frame to IndexedDB (RxDB and Dexie) before the UI
  shows it. The owner reports that this makes the frontend slow.
- Model text reaches the UI after up to 750 ms of buffering in the account worker
  (`account_worker.rs`) and a 200 ms minimum frame interval (`change_notifications.rs`).
- The live `canvas.sqlite3` is 29.4 GB and `analytics.sqlite3` is 31.8 GB. The
  conversation is stored in the provider file, `runtime_items`, and transcript rows in
  `sync_entities`. `analytics_items` (8.1M rows, about 480 compressed bytes each) holds
  item timings, not message text, and the transcript reads reasoning timings from it.

## Decision

### Processes and ownership

```
Server A (one process)  <== QUIC (studio-peer/1) ==>  Server B
   ^ loopback HTTP + SSE            ^ HTTPS + SSE through Tailscale Serve
Desktop window (Tauri)          Phone (PWA) or UI-only computer
```

| Part             | Owns                                                                                        | Does not own                                      |
| ---------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| Server           | State, commands, agents, queue, conversation, drafts, uploads, sync, links to other servers | Microphone, windows, operating system permissions |
| Frontend         | Rendering, thin cache, unsent input buffer                                                  | Business logic                                    |
| Tauri shell      | Window, microphone, dictation, file dialogs, quick look, notifications                      | State and storage                                 |
| Peer link        | Transport between servers                                                                   | Logic                                             |
| Provider adapter | Translation to the provider protocol, a list of capabilities                                | Queue and state                                   |

- The server serves the frontend. The browser, the PWA, and the Tauri window load the same code.
- A remote frontend connects directly to each server, with its own key for each server.
  It does not go through another server.
- The servers maintain the list of servers together. Each server shares its paired
  servers, and a frontend learns the full list and gets UI access to each server from
  any server that it reaches.
- The PWA and Web Push stay.

### One command path

`POST /api/commands {id, kind, target, body}` writes one `runtime_commands` row in the
handler's transaction. The command is a sync entity with the state `pending`, `applied`,
`failed`, or `unknown`.

- The same id with a different body is a conflict. An `unknown` command never runs again by itself.
- A command that changes several chats (stop with descendants, delete) is one row in one transaction.
- `interrupt` names a `turn_id`. A repeat is harmless, so it needs no uncertain state.
- `withdraw(message_id)` deletes a queued message, or interrupts the turn that holds it. Its text returns to the composer as a server draft.
- Stop of a turn does not pause the agent and does not clear the queue. Pause is a separate command.
- Exactly-once applies only to the write to the provider.

The old ledgers move to this table one at a time. Each old table is deleted after its move.

### Two streams to the UI

1. **Durable:** `sync_entities` with the existing `since:{workspaceId,seq}` cursors. It gives a delta after a reconnect, or a snapshot when the cursor has expired.
2. **Live:** model deltas from account worker memory, without SQLite and without the 750 ms timer. A lost delta is harmless, because the final item comes through the durable stream.

Large text (tool output, long messages) is sent as a hash and a size. The client fetches
it once, when it opens it.

### Frontend cache

- A frontend on the same machine as its server keeps no cache.
- A remote frontend (phone, UI-only computer) keeps one IndexedDB record for each
  subscription: its data and its cursor in one value. It writes at most every 5 seconds
  and when the page becomes hidden, after the UI has shown the data. RxDB and Dexie are not used.
- A lost cache write only makes the next reconnect request a larger delta.
- Unsent input is written at once and removed when the server confirms the command.

### Transport

fetch and SSE now. WebTransport may come later as a second binding of the same command and
event contract: for upload streams from the client, binary data, or poor mobile networks.
It needs a `tailscale cert` certificate, an `h3` ALPN, a UDP port, and the HTTPS fallback.

## Data-loss rules

- Do not delete a "copy" of data before a check shows that the data exists elsewhere
  (transcript rows in `sync_entities`, client stores).
- Rewind, restore, and delete make a layr snapshot first. Items are hidden, not deleted.
- The device keeps unsent input until the server confirms it.
- Blob order: write the blob, fsync, then write the row. Blob collection uses mark and sweep with a delay.
- Move client drafts, uploads, and recordings to the server before the client store is deleted.
- Move receipt tables in one transaction. Keep the old tables read-only until they are empty.

## Order of work

1. The desktop window uses only its local server. The frontend reads data from memory. IndexedDB becomes the thin cache.
2. The live delta stream.
3. `runtime_commands` with `interrupt`, `withdraw`, and the queue actions.
4. Move drafts and uploads to the server. Delete the `client-core` stores and `client-wasm`.
5. Move the remaining ledgers, then the agent writers (about 100 files), to command and provider handlers.
6. One copy of the conversation, and blobs. Measure `dbstat` on an APFS clone first.
7. WebTransport, only if a measurement requires it.

## Open questions

- **Analytics size:** `analytics.sqlite3` is 31.8 GB, but `analytics_items` explains only
  about 4 GB. Measure `dbstat` on an APFS clone before any analytics change.
- **Ownership:** most steps change backend code that Sasha owns (#37). Agree each step with him.
- **Upgrade:** a protocol change needs all paired servers on the same version.
