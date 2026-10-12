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

Profile text uses Rust `trim()`; it intentionally does not emulate Python `str.strip()` for U+001C–U+001F.

## Progress panel files

The progress panel operations mirror the Python file checks: progress and agent
directories must be ordinary directories
(`workspaces/runtime/apps/server/src/codex_progress.py:22-57`); the progress
file must be a regular file and is bounded and checked for a stable read
(`workspaces/runtime/apps/server/src/codex_progress.py:115-149`); layout files
are checked as regular files before and after opening
(`workspaces/runtime/apps/server/src/codex_progress_layout.py:36-59`). Layout
writes flush the temporary file and recheck the progress revision before
replacement (`workspaces/runtime/apps/server/src/codex_progress_layout.py:203-231`).
This Rust implementation uses ordinary path operations and does not use
anchored `openat` access. On Windows, progress
read and layout write report the named 501 dependency
`resource-policy/windows-file-identity` until compatible file identity support
is implemented.

The plan that assigns services to slices is
[`docs/plans/rust-runtime-work-breakdown.md`](../../../../docs/plans/rust-runtime-work-breakdown.md).
