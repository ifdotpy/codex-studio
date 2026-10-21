# studio-runtime-schema

## Change Contract

- **Responsibility:** Compile the committed OpenAPI and persisted-state JSON
  schemas once, and provide generated shared runtime values to Rust consumers.
- **Public interface:** `initialize`, `validate`, `registry_for_document`,
  `OPENAPI_URI`, the `JsonSchemaRegistry` type alias, and values in
  `shared_values`; `DeliveryMoment` and its explicit legacy parser in
  [`src/delivery.rs`](src/delivery.rs).
- **Boundaries:** This crate owns no SQL, records, route handlers, or request
  route parsing. The shared delivery contract owns canonical timing and legacy
  spelling normalization. `runtime-store` owns stored-object validation through `Table<R>`;
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
This crate owns the shared runtime contracts: `src/shared_values.rs`,
`src/agent_limits.rs`, `src/api_schema.rs`, `src/multi_server_protocol.rs`,
`src/sync_protocol.rs`, `src/delivery.rs`, and `src/token_rate_limits.rs`. The `studio-contracts`
binary (`src/contracts.rs`) renders their TypeScript, Swift, and VM guest copies
and the web API types of `runtime-http/openapi.json`, and holds the committed
OpenAPI and persisted-state schemas to these owners. Refresh the copies with
`pnpm run contracts:generate`; `pnpm run contracts:check` rejects drift. The
JSON schemas in `src/schemas` are committed source.

`DeliveryMoment` controls when user sends and agent messages or sends may reach
the recipient. Serialization always uses the canonical spelling. Request and
persisted-event readers call `parse_legacy` to accept historical spellings
without rewriting existing events; unknown values fail. The legacy mapping
stays available while existing clients and event histories need it. The
contract renderer checks every OpenAPI delivery field against this owner.
The `delivery::compatibility` serde codec normalizes legacy spellings explicitly
at ingress while the enum's own deserializer remains strict. Persisted metadata
uses the same codec, preserving missing or null timing and tolerating unrelated
metadata fields.
Parsed message callers use `DeliveryMoment::for_message` to apply that same
policy directly; persisted-event readers keep `for_event` for serialized text.
The per-turn attachment item budget is owned by
[`MAX_TURN_ATTACHMENTS`](src/shared_values.rs), with generated Python and web copies.
The focused schema tests also compare the Python event reader to this owner on
shared default, override, legacy, and invalid-input cases using Python's standard
library and an in-memory SQLite database.

`src/task_results.rs` owns optional task result contracts and their limits.
The runtime uses the existing `jsonschema` crate from Cargo.lock (0.49.9).
Task create and update compile the schema before the task changes.
The supported draft 2020-12 subset includes:

- Object properties, required properties, additional properties, and property names.
- Types, enum, const, numeric bounds, and string length bounds.
- Array items, prefix items, contains, uniqueness, and length bounds.
- Object size bounds, dependent properties, and dependent schemas.
- allOf, anyOf, oneOf, not, if, then, and else.
- Boolean schemas and standard description fields.

The runtime refuses references, regular expressions, other drafts, and unknown keywords.
The owner limits serialized schema size to 32,768 UTF-8 bytes and schema depth to 32.
The owner limits structured results to 128,000 UTF-8 bytes.
Submit accepts a JSON value or JSON text that validates against the task schema.
A directly valid string stays a string.
Without a schema, submit continues to require text.

Each result record keeps its display text and adds the validated JSON under `result`.
Task get and history return that JSON value.
Spawn includes the task schema in the worker's first assignment.
Task assignment events include the schema and submit instructions.

A refused submission does not change the task, its version, or its result history.
The runtime stores separate refusal receipts and a failure count.
A repeated request identity returns its receipt without another failure count.
After three failures, the runtime refuses further submissions and emits one `work_result_invalid` event to the lead.
The attempt identity includes the task version, worker epoch, and provider attempt identity.
A lead update or a new provider attempt permits another set of submissions.

The runtime applies the tool contract to both native and legacy tool catalogs.
OpenAPI parity checks use this owner, and `studio-contracts` generates the web types.
