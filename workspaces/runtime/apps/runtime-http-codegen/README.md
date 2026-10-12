# `studio-runtime-http-codegen`

## Change Contract

- **Responsibility:** regenerate checked-in Rust DTOs from the canonical HTTP
  OpenAPI document and detect generation drift.
- **Public interface:** `cargo run --locked -p studio-runtime-http-codegen` and
  the same command with `-- --check`.
- **Forbidden dependencies:** Python runtime, application state, network calls,
  and runtime-server behavior.
- **Invariants:** use `openapi-to-rust` 0.22.0 exactly; write only generated
  files beneath runtime-http's `src/generated` directory.
- **Configuration owner:** repository root `pnpm run api:generate` and
  `pnpm run api:check`.
- **Focused check:** `cargo run --locked -p studio-runtime-http-codegen --
--check`.

The generator is linked as a library dependency, not installed as a global CLI.
Its exact version is pinned in this crate's manifest and resolved by the root
Cargo.lock, so generation provisions the same version in CI and local clones.
