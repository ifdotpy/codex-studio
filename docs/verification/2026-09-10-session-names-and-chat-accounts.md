# Session names and new-chat accounts

## Changes

Managed session names are copied from Studio to native Codex metadata with
`thread/name/set`. This includes generated titles, manual names, existing
sessions and replacement native threads after an account transfer. The stored
receipt includes the account, thread and name. A stale response cannot confirm a
newer name. Native failure retains the Studio name and retries with backoff.
Unchanged names do not produce repeated RPCs. No model turn starts for this copy.

New chats choose the destination project's saved account, then the global
default. They do not inherit an unrelated previous chat's account. Explicit
account choices retain their normal admission checks. An explicit
`reuse_empty: false` preserves the requested new-chat identity.

## Evidence

- Seven isolated contracts passed in both the active checkout and delivery tree.
  They cover title races, native failure, account isolation, generated titles,
  destination account defaults, retries and explicit new-chat identity.
- The installed Codex 0.153.4 accepted two native name updates through Studio's
  synchronization path. Native metadata reads matched. Provider requests: zero.
- Eleven account runtime contracts and 22 account/project contracts passed.
- The project-tree browser suite passed. Its Show more assertion now waits for
  the asynchronous render before counting rows; the expected count is unchanged.
- The production frontend build passed.

The reported denial was reproduced through the active HTTP server before the
update. The same request ID succeeded afterward with the global default account
and the requested project directory. A hidden browser retried that saved request,
opened the chat, cleared the pending request and enabled its composer. No user
message or model turn was submitted.

The active server update replaced two verified method versions and preserved all
four native account connections. A follow-up updated the explicit empty-chat
reuse guard. All 87 eligible existing native sessions received their Studio names
without recorded synchronization errors. Direct native reads of two sessions
also matched their Studio titles. No active agents or monitors were restarted.
