# studio-runtime-providers

## Change Contract

- **Responsibility:** Synchronous JSON-line framing and request lifecycle state for Codex app-server and the Claude bridge.
- **Public interface:** Pure `SessionState` events/actions, `encode_line`, typed `Frame`/`Method`/`RequestId`, `Inbound`, and `PendingBook` transitions.
- **Boundaries:** The caller owns process I/O, account discovery, durable settlement, callback delivery, and backpressure. This crate does not spawn processes or access HTTP or SQLite.
- **Invariants:** Initialization precedes ordinary requests; tickets include their connection generation; registered IDs correlate replies. Deadlines and transport loss preserve uncertainty without retries, and late matched replies remain evidence.
- **Configuration owners:** [`codec.rs`](src/codec.rs) owns methods and frame limits; [`provider_kind.rs`](src/provider_kind.rs) owns provider identity and wire profiles; callers supply monotonic deadline values.
- **Focused check:** `cargo nextest run --build-jobs 4 -p studio-runtime-providers`.

## Framing and request state

The journal decoders accept one complete stdout record and preserve its cursor,
generation, and raw bytes on malformed input. `encode_line` emits one JSON object
and a trailing LF. Replies require exactly one of `result` or `error`; an explicit
`result: null` is valid. Unknown methods and extension fields
survive a decode/encode round trip. Request parameters and response bodies
remain `serde_json::Value`; the codec validates their envelopes.

[`PendingBook`](src/session.rs) distinguishes matched, late, duplicate, and
unknown response IDs. Transport loss closes the book and marks unresolved
requests unknown. A new connection uses a new book and generation.

[`SessionState`](src/connection.rs) emits initialization first. Its separate
`InitializationStatus` tracks the matching reply and the `initialized` write
receipt. The caller must confirm that write before reporting initialization as
complete. `expire` marks only due requests unknown; transport loss enters
`Lost`. `receive_at` uses the transport arrival timestamp to classify a reply,
including one processed after its deadline. Neither path repeats the request.
Claude's wire profile consumes the shared
[method policy](../runtime-schema/src/provider_method_policy.rs) to serialize
selected operations within each thread while allowing independent threads to
proceed. `studio-contracts` generates the bridge's copy of the same policy. Cancellation and definitely unsent
rejection release that thread's lane.

## Callers and checks

The [provider session task](../runtime-services/src/provider_session_task.rs)
drives these actions. The [process adapters](../runtime-process/README.md) own
stdio and supervisor transport. The [Claude bridge](../../../providers/apps/claude-bridge/README.md)
owns SDK translation.

[`tests/codec.rs`](tests/codec.rs) covers journal records, malformed envelopes,
frame limits, unknown fields, and exact encoding. [`tests/connection.rs`](tests/connection.rs)
covers initialization receipts, deadlines, generation fencing, cancellation,
restored requests, and per-thread ordering. Its replay witnesses consume
[recorded JSON-RPC frames and replies](tests/fixtures/provider-replay/), checking
round trips through the codec and delivery through the session. Missing and
orphaned reply fixtures exercise session outcomes directly. These local fixtures
do not verify a live provider request or reset-credit redemption.

Dependency versions are owned by [Cargo.toml](Cargo.toml) and the
[workspace manifest](../../../../Cargo.toml).
