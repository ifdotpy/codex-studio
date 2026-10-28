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
| Frontend         | Rendering, thin cache, unsent input buffer, server keys                                     | Business logic                                    |
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
- Server keys: each frontend keeps one non-extractable WebCrypto key for each server in
  IndexedDB, as today. The PWA and the Tauri window use the same code. The Tauri shell
  stores nothing.

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

- `edit(turn N)` works the same for every provider: hide the items after turn N, restore
  the files from a layr snapshot of turn N, and fork the provider thread from turn N. This
  gives Codex an in-place rewind and replaces the Claude-only rollback.

The old ledgers move to this table one at a time. Each old table is deleted after its move.

### Provider adapter

The adapter translates commands to the provider protocol and reports its capabilities:
steer, rewind, fork, model change, effort, compaction, approvals. The UI shows only the
controls in that list. The queue, the steer queue (`unappliedSteers`), and rollback move
from the Claude bridge to the server.

### Two streams to the UI

1. **Durable:** `sync_entities` with the existing `since:{workspaceId,seq}` cursors. It gives a delta after a reconnect, or a snapshot when the cursor has expired.
2. **Live:** model deltas from account worker memory, without SQLite and without the 750 ms timer. A lost delta is harmless, because the final item comes through the durable stream.

Traffic to a remote frontend stays small:

- Large text (tool output, long messages) is sent as a hash and a size. The client fetches
  it once, when it opens it.
- The stream is compressed (brotli or zstd).
- A chat sends only its visible tail. Older items come on scroll.
- Live deltas go only to the chat that is open.

### Frontend cache

- The IndexedDB uses are the thin cache, the unsent input buffer, and the server keys.
- A frontend on the same machine as its server keeps no cache.
- A remote frontend (phone, UI-only computer) keeps one IndexedDB record for each
  subscription: its data and its cursor in one value. It writes at most every 5 seconds
  and when the page becomes hidden, after the UI has shown the data. RxDB and Dexie are not used.
- A lost cache write only makes the next reconnect request a larger delta.
- Unsent input is written at once and removed when the server confirms the command.

### Routes and reconnect

These rules come from T3 Code (`docs/internals/remote.md` and
`docs/internals/connection-runtime.md` in `pingdotgg/t3code`).

- **A list of routes for each server.** A server can have several addresses: the LAN
  address, the Tailscale Serve address, and later others. The frontend keeps an ordered
  list and uses the first route that answers.
- **Identity before credentials.** Before it sends a key or a session, the frontend asks
  the route for the server's public identity. A LAN address that now belongs to another
  machine receives nothing.
- **Learned routes.** A connected server reports its current LAN and Tailscale addresses.
  The frontend adds them as learned routes and drops the ones the server stops reporting.
  A key that was paired over HTTPS never travels over plain HTTP.
- **Return to the faster route.** While it uses a later route, the frontend checks the
  earlier routes when the network changes, when the app returns to the foreground, and
  every minute. It moves back when one works. A route that fails waits for a cooldown.
- **One owner of retries.** Transient failures retry with jittered exponential backoff,
  capped at 5 minutes, that resets only after the connection stays up. Offline and
  authentication failures wait for a wakeup (network change, foreground, user retry).
- **Probe, then replace.** On foreground or a network change, the frontend probes the
  open stream. Only a failed probe replaces it. A socket that the operating system killed
  during suspension produces no close event, so a probe that gets no answer within a
  short deadline replaces the stream.
- **Health and freshness are separate.** An open stream is not "ready" until the first
  baseline arrives. Cached data stays readable offline, is marked stale, and never
  overwrites newer live data.

Scoped permissions (read only, no terminal) are not needed for one owner. Add them when
a second user or a read-only device appears.

### Transport

fetch and SSE now. WebTransport may come later as a second binding of the same command and
event contract: for upload streams from the client, binary data, or poor mobile networks.
It needs a `tailscale cert` certificate, an `h3` ALPN, a UDP port, and the HTTPS fallback.

### Storage

SQLite stays. One user and one writer actor do not need a database server, and the state
is one file for backups and layr snapshots. The problems are in its use:

- No I/O inside a write transaction. Long transactions block every writer; one took 282 s
  (`docs/verification/2026-10-05-sqlite-owners-and-code-cells.md`).
- An alert when a write transaction exceeds a set duration.
- `canvas.sqlite3` has about 4 GB of free pages. Reclaim them after the copies are removed.

### Types

Invalid state must be impossible to express in Rust, TypeScript, and SQLite.

- **One owner: the Rust type.** TypeScript types come from it through the generated
  OpenAPI contract (`pnpm run contracts:generate`). SQLite constraints come from it too:
  a generated check trigger for each enum column, and a parity check that fails when a column
  and its Rust enum differ. No language keeps a hand-written copy of a value list.
- **Enums with data instead of a status string and optional fields.** A state is one
  variant that carries exactly the fields it needs.
- **Few `Option`s.** `Option` only where absence is a real state of the domain, never for
  "not loaded yet" or "old format".
- **No free strings.** Identifiers are newtypes (`AgentId`, `TurnId`, `RequestId`) in Rust
  and branded types in TypeScript. Closed sets are enums.
- **Strict decoding.** Rows decode with `serde::Deserialize` straight into the type,
  without an intermediate `serde_json::Value`. An unknown variant is an error, not a
  default. Old data changes through a migration, not through a fallback in the decoder.
- **Columns for state, a typed payload for content.** State and everything that is
  searched or filtered are typed columns. The SQLite form of an enum is a variant column
  plus columns that are required for that variant and forbidden for the others. Nested
  content that is only shown (tool call arguments, item bodies) is one JSON payload
  column that decodes straight into a struct.
- **Checks as generated triggers.** SQLite cannot change a table `CHECK` without a table
  rebuild, and a rebuild of a large table (`runtime_items` has 1.5M rows) blocks the
  single writer. The checks are generated `BEFORE INSERT` and `BEFORE UPDATE` triggers,
  which a migration replaces in milliseconds.
- **Strict inside, tolerant at the provider boundary.** Codex and Claude add item types
  in new releases. Provider data decodes into a variant `Unknown { kind, raw }` when its
  type is new. Studio stores it and shows it as is. Inside Studio an unknown variant is
  an error.
- **Internally tagged enums only** (`#[serde(tag = "kind")]`). Other serde forms produce
  poor generated TypeScript types.
- **Version skew.** An older server cannot decode a new variant from a newer peer. All
  paired servers update together.

Example, the delivery state of a message:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Delivery {
    Queued,
    Submitted { turn: TurnId },
    Applied { turn: TurnId, at: Timestamp },
    Withdrawn { at: Timestamp },
    Unknown { since: Timestamp },
}
```

```sql
-- Columns: delivery TEXT NOT NULL, turn TEXT, at REAL, since REAL.
CREATE TRIGGER messages_delivery_insert BEFORE INSERT ON messages
WHEN NOT (
  NEW.delivery IN ('queued','submitted','applied','withdrawn','unknown')
  AND (NEW.delivery IN ('submitted','applied')) = (NEW.turn IS NOT NULL)
  AND (NEW.delivery IN ('applied','withdrawn')) = (NEW.at IS NOT NULL)
  AND (NEW.delivery = 'unknown') = (NEW.since IS NOT NULL)
)
BEGIN SELECT RAISE(ABORT, 'invalid delivery state'); END;
-- The same trigger exists for BEFORE UPDATE.
```

### One owner for each kind of data

| Data                           | Copies now                                                                                                           | Single owner                                                  | The others                                                                                   |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Conversation                   | Provider file, `runtime_items`, transcript rows and cached pages in `sync_entities`                                  | `runtime_items`                                               | The provider file stays as the provider's own cache. Transcript pages are built at read time |
| Large text                     | Inside item records and agent JSON                                                                                   | `blobs/`, addressed by hash and compressed                    | Rows keep the hash and the size                                                              |
| Server projections on a client | Client SQLite in `client-core`, RxDB in the web client                                                               | The server                                                    | A remote frontend keeps only the thin cache                                                  |
| Command results                | About 10 receipt tables (`runtime_operation_receipts` has 2.83M rows for 3,378 chats)                                | `runtime_commands`                                            | Old results compact to a fingerprint after the existing expiry                               |
| Outgoing requests              | 4 outboxes (`runtime_server_outbox`, `runtime_federation_outbox`, `runtime_access_outbound`, `runtime_server_fetch`) | One outbox with `kind` and `target`                           | The same request id and exact body on each retry                                             |
| Signatures and nonces          | 2 schemes and 2 nonce tables                                                                                         | `studio-multi-server-v1`, with federation as a principal kind | Separate trust lists stay                                                                    |
| Drafts                         | 9 client modules, the Rust client store, and the server                                                              | The server                                                    | The device keeps only unsent input                                                           |

### Removed parts

- The Electron application and its Python helpers (`recover_backend.py`, `deliver_server.py`,
  `desktop_delivery.py`, `install_update.py`). Tauri replaces them.
- `workspaces/runtime/apps/server/`, which holds only untracked Python caches.
- `client-wasm`, and the `client-core` stores, sync, outbox, and transport.
- The peer update actions (`peer_updates.rs`, `peer_update_pull.rs`, about 2.5k lines)
  move to the shared outbox and commands.

## Data-loss rules

- All conversation history stays: messages, items, and items hidden by a rewind (owner
  decision, 2026-10-28). Only service data is removed: expired receipts, tombstones,
  cached transcript pages, used nonces, completed outbox entries, and client copies.

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
7. Routes and reconnect rules in the frontend.
8. WebTransport, only if a measurement requires it.

## Risks

- Complexity grows before it falls: old and new paths live together for months.
- The UI depends on its server. A server restart breaks the stream for a short time.
- Agents survive a server restart only with the supervisor, which Windows does not have.
- Disk use grows: rewind hides items and makes snapshots.
- The UI moves to Tauri now and to Attar later. The frontend contract must suit Attar,
  or the rendering code is rewritten twice.

## Open questions

- **Analytics size:** `analytics.sqlite3` is 31.8 GB, but `analytics_items` explains only
  about 4 GB. Measure `dbstat` on an APFS clone before any analytics change.
- **Ownership:** most steps change backend code that Sasha owns (#37). Agree each step with him.
- **Upgrade:** a protocol change needs all paired servers on the same version.
