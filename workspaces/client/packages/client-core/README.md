# Native client core

Owns the installed client's durable state, network and presentation services.
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

Entity batches are applied and materialized atomically by
[`projections.rs`](src/projections.rs). The derived state sequence is a durable
local revision; entity sequences and the pull checkpoint remain server cursors.
[`sync.rs`](src/sync.rs) stages complete pulls before committing, requests reset
support, and replaces entity rows and cursors together after a tombstone reset.
A pull whose local base revision changed fails without writing and can be retried.
Focused regression checks: `just test studio-client-core entity_`.
