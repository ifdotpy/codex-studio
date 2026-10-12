# `studio-runtime-services`

## Change Contract

- **Responsibility:** the runtime's application rules: what a request or an
  event does to state, in which order, and what it returns. One leaf module
  per service.
- **Public interface:** each service module's functions and port traits.
- **Allowed dependencies:** `studio-runtime-store`, `studio-runtime-providers`,
  `studio-runtime-process`, `studio-operations`.
- **Forbidden dependencies:** `studio-runtime-http`, Axum, Tokio, the server
  application, the Python runtime.
- **Invariants:** synchronous code only; no service holds a store transaction
  across a call into a provider or process port; an outcome that cannot be
  observed is reported as unknown and never retried here.
- **Focused check:** `cargo nextest run -p studio-runtime-services`.

## Pattern

A service module separates pure decisions from effects. Pure functions take
snapshots and explicit clocks and return decisions; they are tested directly.
Effects go through the store's repositories or through a narrow port trait
declared in the module, with an in-memory fake for tests. A route module in
`studio-runtime-http` converts the request DTO to the service input, calls the
service inside `tokio::task::spawn_blocking`, and converts the result or error
to the response DTO.

Dependency direction:

```text
operations ← store, providers, process ← services ← http ← runtime-server
```

The workspace file service resolves relative paths under the agent's `cwd`.
It also accepts absolute paths and follows symlinks without checking that the
resolved file stays inside `cwd`, matching Python's existing
[`workspace_path`](../../apps/server/src/codex_workspace.py#L567-L572)
rule. Keep this behavior deliberate; tightening it requires a separate
compatibility decision. The service pins it in
`workspace_files::tests::local_file_metadata_and_content_follow_symlinks`.

The plan that assigns services to slices is
[`docs/plans/rust-runtime-work-breakdown.md`](../../../../docs/plans/rust-runtime-work-breakdown.md).
