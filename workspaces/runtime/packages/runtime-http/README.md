# `studio-runtime-http`

## Change Contract

- **Responsibility:** own the committed OpenAPI authority, generated Rust DTOs,
  schema identity, Axum router and HTTP boundary compatibility rules.
- **Public interface:** `router`, `HttpState`, `api_schema_hash`, and
  `operations`.
- **Forbidden dependencies:** Python runtime modules, SQLite, provider SDKs,
  and the runtime-server application.
- **Invariants:** schema bytes stay canonicalizer-v3 stable; documented but
  unimplemented operations return the common 501 error; synchronous domain
  interfaces remain outside the async edge.
- **Configuration owner:** `studio-runtime-server` supplies listener identity,
  process token, workspace identity, and local diagnostics to `HttpState`.
- **Focused check:** `cargo nextest run -p studio-runtime-http`.

`openapi.json` is the API schema authority and outlives the Python application.
Python export parity is checked by `pnpm run api:check`. Rust DTO drift is
checked by the crate's generator test using `openapi-to-rust` 0.22.0.

Direct dependencies introduced by this package: Axum 0.8.9 (MIT), Tokio
1.53.2 (MIT), SHA-2
0.10.9 (MIT OR Apache-2.0; schema hash), subtle 2.6.1 (MIT OR Apache-2.0;
constant-time token comparison), and openapi-to-rust 0.22.0 (MIT; test-only
DTO drift generator). Serde and
serde_json are existing workspace dependencies.

Regenerate DTOs with `openapi-to-rust generate
workspaces/runtime/packages/runtime-http/openapi.json --output-dir
workspaces/runtime/packages/runtime-http/src/generated --module-name generated
--types-only`. `cargo nextest run -p studio-runtime-http` checks freshness.
