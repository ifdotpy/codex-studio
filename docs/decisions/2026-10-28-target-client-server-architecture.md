# Target client and server architecture

- **Status:** Proposed, 2026-10-28. The owner accepted the client model (one frontend, thin cache), the domain model, the type rules, and the command execution rules (no event store). The other parts wait for review.
- **Audience:** maintainers who plan backend, client, and multi-server work.

## In short

One server process on each machine owns all state and all business logic. Every UI (the
desktop window, a browser, the phone) is one frontend that renders server data and sends
commands. The frontend keeps only a thin cache and a buffer of unsent input. Every action
of a user, an agent, or a peer server is one command with a stable id. Its result lives in
the row of the entity that it changes.

## Context

Facts measured on 2026-10-28 at `85663a40a`:

- About 10 receipt tables, 4 outboxes, and 2 signature schemes (`studio-multi-server-v1`,
  `studio-federation-v1`) do the same job for different features.
- A user message has about 6 state vocabularies with about 11 statuses, in the browser,
  SQLite, the account worker, and the Claude bridge.
- Stop takes 5 to 6 hops to the provider, has no request id, pauses the agent, and
  cancels the whole queue. A queued message cannot be withdrawn after `reserved`.
- One `runtime_agents` JSON record holds the chat, the current provider run
  (`startAttempt`, the interrupt ticket), and scheduling state. About 100 files write it.
- Entity publication copies `runtime_agents`, `runtime_requests`, and `runtime_work` rows
  into `sync_entities` (1.73M rows) in each writer transaction.
- `client-core` (Rust, about 19.9k lines) does not depend on any runtime crate. The v1
  signature and the paired transport exist in both. On a machine with a server, the
  client keeps a second SQLite copy of the server projections.
- Each frontend pairs separately with each server and keeps one key for each.
- The web client writes every stream frame to IndexedDB (RxDB and Dexie) before the UI
  shows it. The owner reports that this makes the frontend slow.
- Model text reaches the UI after up to 750 ms of buffering in the account worker
  (`account_worker.rs`) and a 200 ms minimum frame interval (`change_notifications.rs`).
- The live `canvas.sqlite3` is 29.4 GB and `analytics.sqlite3` is 31.8 GB. The
  conversation is stored in the provider file, `runtime_items`, and transcript rows in
  `sync_entities`. `analytics_items` (8.1M rows, about 480 compressed bytes each) holds
  item timings, not message text, and the transcript reads reasoning timings from it.
- The schema has no foreign keys. See the [schema overview](../audits/db-schema-2026-10-28.md).

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
| Frontend         | Rendering, thin cache, unsent input buffer, device key                                      | Business logic                                    |
| Tauri shell      | Window, microphone, dictation, file dialogs, quick look, notifications                      | State and storage                                 |
| Peer link        | Transport between servers                                                                   | Logic                                             |
| Provider adapter | Translation to the provider protocol, a list of capabilities                                | Queue and state                                   |

- The server serves the frontend. The browser, the PWA, and the Tauri window load the same code.
- A remote frontend connects directly to each server. It does not go through another server.
- The PWA and Web Push stay.

### Domain model

Today one agent record mixes three things. They become separate entities, each with an
enum state:

| Entity         | Meaning                                                                 | Examples of state                                                          |
| -------------- | ----------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| **Chat**       | The conversation that the user sees, with its settings and its queue    | `Active`, `Paused`, `Archived`                                             |
| **Turn**       | One provider run inside a chat                                          | `Starting`, `Running { session }`, `Completed`, `Failed`, `Interrupted`    |
| **Session**    | One provider process or thread that runs turns                          | `Starting`, `Ready`, `Lost`                                                |
| **Message**    | Input from the user, an agent, or a peer, with its delivery state       | `Delivery` (see Types)                                                     |
| **Item**       | One record of the conversation that a turn produced                     | Final, or hidden by a rewind                                               |
| **Approval**   | A provider request for permission or an answer, inside a turn           | See Approvals                                                              |
| **Process**    | A command that Studio starts and watches for an agent (today a monitor) | `Starting`, `Running`, `Completed { exit }`, `Failed`, `Cancelled`, `Lost` |
| **Work**       | An assignment from a lead to a worker, with its result                  | `Ready`, `Running`, `Review`, `Blocked`, `Accepted`, `Cancelled`           |
| **Checkpoint** | A git restore point of a turn                                           | Immutable                                                                  |
| **Attachment** | An uploaded file of a message                                           | Immutable; the file on disk is the owner                                   |

- Commands point to real rows: `interrupt(turn)`, `edit(turn)`, `Delivery::Submitted { turn }`.
- A chat has at most one turn that is not finished. Its state, not fields inside a JSON
  record, says whether the chat is busy.

Other tables today, measured on 2026-10-28:

| Table today                                                  | Rows        | Target                                                                                                                                                            |
| ------------------------------------------------------------ | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `runtime_tasks`                                              | 1.12M       | Not an entity. One row for each command or tool call item, with the command, status and output tail (estimate about 5 GB). The status becomes a field of the Item |
| `runtime_tool_requests`, `runtime_tool_results`              | 218k each   | The tool call Item and its result                                                                                                                                 |
| `runtime_monitors`                                           | 126k        | Process                                                                                                                                                           |
| `runtime_completed_turns`                                    | 94k         | The `Completed` state of Turn                                                                                                                                     |
| `runtime_execution_attempts`, `_effects`, `_inputs`, `_runs` | 57k to 112k | Turn attempts and the outbox                                                                                                                                      |
| `runtime_rooms`                                              | 2.6k        | Derived from the chat tree (two agents, or one root). Not stored                                                                                                  |
| `runtime_user_tasks`                                         | 56          | No writer or reader was found. Remove after a check                                                                                                               |
| Drafts in `sync_documents`                                   | 1.3k        | A draft field of Chat                                                                                                                                             |

The data-loss rules apply: a table that is a copy is removed only after a check shows
that its owner holds the same data. For `runtime_tasks` this is not proven yet: in a
sample of 6 rows, the output tail matched the item text only in part.

### Approvals

Today approvals and provider questions share `runtime_requests` (297 rows) with string
states and one answer route. The target makes it one entity, a child of Turn:

```rust
enum Request {             // fixed at creation, decides the allowed answer
    Exec { command, cwd, reason },
    Patch { item: ItemId, files },
    Permissions { requested },
    Elicitation { schema },
    Question { questions },
    ToolUse { tool, input },   // Claude
    AgentQuestion { .. },      // today agent/asyncQuestion
    MonitorStart { .. },       // today monitor/approve
    BrowserOpen { .. },        // today browser/open
}
enum ApprovalState {
    Pending,
    Answering { answer: Answer, command: CommandId },  // outbox entry exists
    Answered { answer: Answer, by: Principal, at: Timestamp },
    Unknown { answer: Answer, since: Timestamp },      // delivery lost
    Closed { reason: CloseReason, at: Timestamp },     // turn ended, provider resolved, session lost, chat deleted, expired
}
```

- The command `answer(approval, answer)` checks that the answer fits the request kind.
  The same answer again replays. A different answer is a conflict.
- An answer is never inferred. When the turn ends or the session is lost, `Pending`
  becomes `Closed` and `Answering` becomes `Unknown`.
- An automatic answer by policy (full access mode) is also a row, created as `Answered`
  with `by: Policy`. Today it leaves no record.
- The Claude timeout in full access mode (10 min today, in the bridge) moves to the
  server as a `Closed { reason: Expired }` with a decline effect.
- All approvals stay in history. Today `deleted` hides a row; it becomes a hidden flag,
  not a state.

### Permissions

Today every UI client, the phone, and a paired device act as the owner. The signed path
checks the device and then runs the handler as the owner, so the handler does not know
the caller. Agents and peer servers have their own checks in three different places.

Target: the edge checks the signature or token and gives one `Principal`. The decide
function checks the command against the rows, without I/O.

```rust
enum Principal {
    Owner,                                  // loopback desktop window
    Device { id: DeviceId, grant: Grant },  // phone, other laptop
    Peer { server: ServerId },
    Agent { id: AgentId, epoch: Epoch },    // tree position comes from the rows
    Federated { peer: PeerId },
    Policy,                                 // automatic answers
}
enum Grant { Full, Observe }                // Observe: read commands only
```

| Command class                               | Who may run it                                                                        |
| ------------------------------------------- | ------------------------------------------------------------------------------------- |
| Read                                        | Everyone, within their own data                                                       |
| Chat control (send, stop, answer, withdraw) | Owner, Device Full. An agent only on its descendants. A peer only on its link workers |
| Spawn                                       | Owner, Device Full, a root lead, a peer for its own link                              |
| Host (exec, git, folders, file write)       | Owner, Device Full, Peer                                                              |
| Pairing, grants, revoke, settings           | Owner only                                                                            |
| Federation                                  | Federated, only in rooms that the peer approved                                       |

- The grant lives in the shared trust list. A server reads the grant from its own copy,
  never from the request.
- The descendant check exists twice today (`managed_host_tools.rs`, `services.rs`). It
  becomes one function.
- Known gap today: a request through Tailscale Serve is trusted when its source is a
  tailnet address (`middleware.rs`, `trusted`). It can read `/api/session`, which returns
  the owner token. The Tailscale login is not compared with the owner, as it is on the
  signed path. Target: Serve requests use device keys and get no owner token.
- A process on the server host can read the owner token through loopback. Agent shells
  therefore have owner rights outside the agent tools.

### One command path

`POST /api/commands {id, kind, target, body}` is the one entry for every action. The UI,
the agent tools (`managed_host_tools`), and peer servers (orchestration) use it. Checks,
idempotency, and states are therefore the same for all callers.

A command is an API concept, not a table. Its result lives in the row of the entity that
it changes:

- **A command that creates a row** (message, spawn, upload): the client id is the row id,
  and the row keeps the hash of the body. The row is the receipt. The same id with a
  different body is a conflict.
- **A command that changes state** is idempotent by its target: `interrupt` names a turn,
  `configure` carries the expected revision, `delete` does nothing when the row is gone.
  It needs no receipt.
- **A separate record** stays only for the cases that have no main row (section 3 of the
  [schema overview](../audits/db-schema-2026-10-28.md)).
- A lost outcome is a state of the row (for example `Delivery::Unknown`). It never runs
  again by itself. Exactly-once applies only to the write to the provider.

Command rules:

- A command that changes several chats (stop with descendants, delete) runs in one transaction.
- `withdraw(message)` deletes a queued message, or interrupts the turn that holds it. Its
  text returns to the composer as a server draft.
- Stop of a turn does not pause the chat and does not clear the queue. Pause is a separate command.
- `edit(turn)` works the same for every provider: hide the items after that turn, restore
  the files from a layr snapshot of that turn, and fork the provider thread from it. This
  gives Codex an in-place rewind and replaces the Claude-only rollback.

### Command execution

The write discipline comes from t3code orchestration-v2 (commit `48a29236`), without its
event store:

1. **Decide without I/O.** A pure function reads the current rows and the command and
   returns the row changes and the outbox entries. It does not call the provider, the file
   system, or the network.
2. **One transaction.** The row changes, the body hash of a create command, and the outbox
   entries are written together. A half-written command cannot exist.
3. **Effects in a worker.** A worker reads the outbox and does the I/O (provider, files,
   peer servers). It reports the result as a new command through the same path.
4. **Recovery after a crash.** The worker marks an outbox entry `Sending` in a commit
   before the I/O. On restart, each `Sending` entry follows the rule of its effect kind:

   | Rule            | Restart action                                                           | Effects                                                                                                                  |
   | --------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------ |
   | `TargetDedupes` | Send again with the same id                                              | Claude turn start and steer, reset credit (idempotency key), federation, replay-safe peer actions, file writes by rename |
   | `AskTarget`     | Ask the target. Found: done. Proven absent: queue again. Else: `Unknown` | Codex turn start (history by `clientId`), peer `exec` (`exec_receipt`)                                                   |
   | `None`          | `Unknown`                                                                | Interrupt, rollback, fork, other peer actions                                                                            |
   - Only the worker sets `Unknown`, in the same transaction as the row state.
   - The user resolves it with one of three commands: `retry`, `mark_delivered`, or
     `discard`. `retry` of an `AskTarget` entry asks the target first and refuses
     while the answer is unclear. `discard` returns a message text to the composer.
   - Without the supervisor (Windows), provider processes end with the server. Their
     `Sending` entries become `Unknown` unless the target answers.

5. **Raw provider frames go to rotating log files**, not to tables. They help debugging
   and replay. They are not a source of state.
6. **Replay tests.** A test feeds recorded commands and provider frames to the decide
   function and compares the resulting rows.

**Rejected: an event store as the source of truth** (t3 keeps state as an append-only
event log and computes tables as projections). The discipline above gives the same
benefits. The event store adds costs that Studio does not need:

- Every old event must stay decodable forever. A format change needs compatibility code
  or a rewrite of history, which conflicts with strict decoding (see Types).
- Current state is computed. A projection cache can diverge from the log again.
- A wrong event cannot be corrected in place, only by a compensating event.

State lives in typed tables. Items stay append-only because they are the conversation
history, but they do not control state.

### Provider adapter

The adapter translates commands to the provider protocol and reports its capabilities:
steer, rewind, fork, model change, effort, compaction, approvals. The UI shows only the
controls in that list. The queue, the steer queue (`unappliedSteers`), and rollback move
from the Claude bridge to the server.

### Two streams to the UI

1. **Durable:** the stream reads the main tables directly. Each main table has a `seq`
   column that a trigger sets from one global counter on each insert and update. A delete
   writes a tombstone. A subscription returns the rows with `seq` above its cursor, or a
   snapshot when the cursor is older than the oldest kept tombstone. `sync_entities` no
   longer keeps a copy of the main rows.
2. **Live:** model deltas from account worker memory, without SQLite and without the 750 ms
   timer. A lost delta is harmless, because the final item comes through the durable stream.

Traffic to a remote frontend stays small:

- Large text (tool output, long messages) is sent as a hash and a size. The client fetches
  it once, when it opens it.
- The stream is compressed (brotli or zstd).
- A chat sends only its visible tail. Older items come on scroll.
- Live deltas go only to the chat that is open.

### Device identity

- Each device (desktop window, phone, browser profile) has one key, a non-extractable
  WebCrypto key in IndexedDB.
- The servers keep together the list of servers and the list of trusted device keys. A
  server shares both with its paired servers. A device that a server trusts gets access
  to every server of the same owner without a separate pairing.
- A revocation is made on any server and reaches the others through the same list. A
  server that was offline applies it when it connects again. Until then it still trusts
  the revoked key.

### Contract version

At each connection, the frontend and each peer compare the hash of the contract (the
existing `api-schema` hash). When the hashes differ, the server refuses the connection
and shows "update needed". It never decodes data in a form that it does not know.

### Frontend cache

- The IndexedDB uses are the thin cache, the unsent input buffer, and the device key.
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
  OpenAPI contract (`pnpm run contracts:generate`). SQLite checks come from it too: a
  generated check trigger for each enum column, and a parity check that fails when a
  column and its Rust enum differ. No language keeps a hand-written copy of a value list.
- **Enums with data instead of a status string and optional fields.** A state is one
  variant that carries exactly the fields it needs.
- **Few `Option`s.** `Option` only where absence is a real state of the domain, never for
  "not loaded yet" or "old format".
- **No free strings.** Identifiers are newtypes (`ChatId`, `TurnId`, `RequestId`) in Rust
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
| Entity state for sync          | Main tables and their copies in `sync_entities`                                                                      | The main tables, with a `seq` column                          | Tombstones for deletes                                                                       |
| Large text                     | Inside item records and agent JSON                                                                                   | `blobs/`, addressed by hash and compressed                    | Rows keep the hash and the size                                                              |
| Server projections on a client | Client SQLite in `client-core`, RxDB in the web client                                                               | The server                                                    | A remote frontend keeps only the thin cache                                                  |
| Command results                | About 10 receipt tables (`runtime_operation_receipts` has 2.83M rows for 3,378 chats)                                | The row of the changed entity                                 | A separate record only where no main row exists                                              |
| Outgoing requests              | 4 outboxes (`runtime_server_outbox`, `runtime_federation_outbox`, `runtime_access_outbound`, `runtime_server_fetch`) | One outbox with `kind` and `target`                           | The same request id and exact body on each retry                                             |
| Signatures and nonces          | 2 schemes and 2 nonce tables                                                                                         | `studio-multi-server-v1`, with federation as a principal kind | Separate trust lists stay                                                                    |
| Device keys                    | One key for each pair of device and server                                                                           | One key for each device, in the shared trust list             | Revocation spreads through the list                                                          |
| Drafts                         | 9 client modules, the Rust client store, and the server                                                              | The server                                                    | The device keeps only unsent input                                                           |

### Removed parts

- The Electron application and its Python helpers (`recover_backend.py`, `deliver_server.py`,
  `desktop_delivery.py`, `install_update.py`). Tauri replaces them.
- `workspaces/runtime/apps/server/`, which holds only untracked Python caches.
- `client-wasm`, and the `client-core` stores, sync, outbox, and transport.
- The entity publication copy into `sync_entities`.
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
- Move a receipt table in one transaction. Keep the old table read-only until it is empty.

## Risks

- Complexity grows before it falls: old and new paths live together for a time.
- The UI depends on its server. A server restart breaks the stream for a short time.
- Agents survive a server restart only with the supervisor, which Windows does not have.
- Disk use grows: rewind hides items and makes snapshots.
- A revoked device key stays valid on a server that was offline until it connects again.
- The UI moves to Tauri now and to Attar later. The frontend contract must suit Attar,
  or the rendering code is rewritten twice.

## Open questions

- **Analytics size:** `analytics.sqlite3` is 31.8 GB, but `analytics_items` explains only
  about 4 GB. Measure `dbstat` on an APFS clone before any analytics change.
- **Ownership:** Sasha handed the typed storage work to Claude. Most other parts also
  change backend code that he owns (#37). Agree each part with him.
- **Phone grant:** should a newly paired phone get `Full` (as today) or `Observe`?
- **Agent shells:** should agents lose the owner token on loopback? Agents with full
  access can also change files and the database directly, so the gain is small.
- **Codex turn start:** does Codex ignore a repeated `turn/start` with the same
  `clientUserMessageId` when the thread is idle? It duplicates input when a turn is
  active (`docs/orchestration.md`). Measure before the rule moves to `TargetDedupes`.
- **Probes:** no read was found that proves whether an interrupt or a rollback was
  applied. Until one exists, both stay `None`.
