# studio-runtime-schema

## Change Contract

- **Responsibility:** Compile the committed OpenAPI and persisted-state JSON
  schemas once and provide validators to runtime HTTP and store crates.
- **Public interface:** `initialize`, `validate`, `registry_for_document`,
  `OPENAPI_URI`, and the `JsonSchemaRegistry` type alias.
- **Boundaries:** This crate owns no SQL, records, route handlers, or request
  parsing. `runtime-store` owns stored-object validation through `Table<R>`;
  `runtime-http` owns request validation and resolves schema references through
  this crate.
- **Invariants:** Schema names resolve only to compiled schemas. An unknown
  name or compilation failure is returned as an error; a schema mismatch is
  returned as `Ok(false)`. OpenAPI and the committed account-registry and
  workspace-usage-limits schema documents remain the schema sources.
- **Configuration owner:** No runtime configuration. Schema document paths
  are fixed by `src/lib.rs` and point into `runtime-http`.
- **Focused checks:** Run `cargo test -p studio-runtime-schema` and
  `cargo check -p studio-runtime-store -p studio-runtime-http` from the
  workspace root.

The crate shares one lazily initialized set of persisted-record validators.
HTTP request validators use `registry_for_document` to prepare references
against the same OpenAPI document and the persisted-state schema documents.
