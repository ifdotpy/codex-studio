# Native client core

Owns the installed client's network and presentation services.
Ported from `codex/attar-rust-migration-20260922` (`6586f3f77`) into the current
workspace. Tauri hosts this core on desktop and mobile; React presents state and
sends commands. Shared client business rules have one Rust implementation.

The native host owns state paths, credentials, platform permissions and server
connections. Web content cannot select another server's database or grant itself
native permissions. The server retains its database and orchestration identity.

Migration acceptance requires the current desktop flows, remote-server isolation,
existing drafts and recordings, and exact mutation identities to survive restart.
The implementation must preserve schema checks, cancellation uncertainty and
receipt reconciliation. Hidden tests must not open dialogs or microphones,
activate applications, or send operating-system input.

The mobile target is an installed application connecting to a paired server.
WASM is not part of this migration. macOS-only services must have explicit mobile
adapters; compiling an unavailable-platform stub does not establish parity.

This package is under integration. Its presence alone does not switch a caller
or establish release readiness.

The core keeps no client state. The server owns drafts, uploads, sync and the
queue (see the [target architecture](../../../../docs/decisions/2026-10-28-target-client-server-architecture.md)).
[`store_move.rs`](src/store_move.rs) moves what the removed client store still
holds (drafts, unsent uploads, recordings) to the server at the first connection
after an application start. It deletes a local row only after it has read the same
data back from the server. A failed read-back keeps the row, and the next start
tries again.
