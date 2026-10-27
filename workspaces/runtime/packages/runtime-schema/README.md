# studio-runtime-schema

## Change Contract

- **Responsibility:** Compile the committed OpenAPI and persisted-state JSON
  schemas once, and provide generated shared runtime values to Rust consumers.
- **Public interface:** `initialize`, `validate`, `registry_for_document`,
  `OPENAPI_URI`, the `JsonSchemaRegistry` type alias, and values in
  `shared_values`; `DeliveryMoment` and its explicit legacy parser in
  [`src/delivery.rs`](src/delivery.rs).
- **Boundaries:** This crate owns no SQL, record persistence, route handlers, or request
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
`src/sync_protocol.rs`, `src/delivery.rs`, `src/provider_kind.rs`,
`src/provider_method_policy.rs`, `src/token_rate_limits.rs`, `src/records.rs`,
and `src/studio_errors.rs`. The `studio-contracts`
binary (`src/contracts.rs`) renders their TypeScript, Swift, and VM guest copies
and the web API types of `runtime-http/openapi.json`, and holds the committed
OpenAPI and persisted-state schemas to these owners. Refresh the copies with
`pnpm run contracts:generate`; `pnpm run contracts:check` rejects drift. The
JSON schemas in `src/schemas` are committed source. Draft payload schemas and
`web/src/generated/rustOwned.ts` are generated from the client-domain Rust owner
with schemars, through the same contract commands. `AgentCommandProgress` and
`AgentStall` in `src/agent_progress.rs` also derive their OpenAPI fragments;
the renderer refreshes those two components before generating API types and identity.

[`src/records.rs`](src/records.rs) owns `WorkStatus` and `WorkspaceMode`.
The store imports these enums; OpenAPI components and HTTP types are generated
from the same owner, including the narrower lead-creation mode set. Checked work
readers require a known status and string-list dependencies. Progress retains its
existing default for an omitted update time but refuses a present invalid or null
value instead of treating it as time zero.

[`src/studio_errors.rs`](src/studio_errors.rs) owns diagnostic kinds, context,
causes, and effect certainty. Its generated contracts are shared by the runtime,
SSE publication, and App errors UI. An error message alone does not prove that
an operation had no effects.

[`src/queue_command.rs`](src/queue_command.rs) owns queue mutation commands.
The HTTP adapter and service consume its strict tagged enum; the contract renderer
derives the queue request components from it. Nullable field presence survives
serialization so existing request signatures and receipt replays stay compatible.

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

`src/usage.rs` owns provider-neutral token observations and the single notification
parser. Response evidence, cumulative counter scope, and missing final usage remain
explicit. `studio-contracts` generates the observation JSON schema, named OpenAPI
components, and the Claude bridge's `usage-contract.generated.ts`. The committed
Codex notification schemas under `src/schemas/codex` come from the pinned vendor
source. The unified provider generator derives the consumed Rust declarations;
freshness checks and actual-consumer mutation tests enforce that relationship.

The [`usage/persisted.rs`](src/usage/persisted.rs) adapter owns legacy analytics
usage column interpretation. It prefers canonical `studioUsage`, preserves
uncached versus inclusive historical input semantics, and reconstructs sparse
legacy Claude notifications for totals-only budget migration. Analytics notice
and usage schema copies are checked against the usage owner's renderer by a
short-tier parity witness.

`parse_legacy_totals` is a transitional total-only projection for pre-contract
Claude bridges and their journal replay. It never invents missing token categories,
accepts no Codex input, and cannot bypass malformed `studioUsage`. Retire it once
those bridges and the replay window have closed.
The provider method policy owns per-thread serialization methods and parameter
predicates. Both the Rust transport and the generated Claude bridge module consume
this owner; contract drift checks cover the bridge copy. Recovery conservatively
reserves serialized-capable methods when their parameter predicate is unreadable.

## Provider contract sources

[`model_catalog.rs`](src/model_catalog.rs) owns the Studio-consumed model row
fields. Provider ingress validates them before cache or admission; nullable
upstream fields preserve omission and explicit null, and unknown metadata stays
opaque. Model names, reasoning effort names and service tier IDs remain open.
The contract renderer generates OpenAPI model rows and the bridge producer's
TypeScript projection from this owner. The picker consumes the generated API
contract instead of reparsing individual capability fields.

The [provider extensibility contract](../../../providers/README.md#provider-extensibility)
owns provider-specific policy. The owners below describe implemented, selected
contracts; they do not replace every vendor or historical document with a fully
normalized model.

[`provider_kind.rs`](src/provider_kind.rs) owns shared provider identity.
The contract renderer generates the web identity module and schema references
from it. The store's
[dated identity migration](../runtime-store/src/migrations/m2026_10_26_provider_identity.rs) converts
historical owned representations before strict current readers run.

The [pinned vendor source](../../../tooling/apps/repository-checks/provider-contract-sources.ts)
owns the Codex version and selected extraction paths. The
[unified generator](../../../tooling/apps/repository-checks/generate-provider-contracts.ts)
derives Rust declarations in [`codex_generated.rs`](src/codex_generated.rs) and
the bridge's selected TypeScript leaves from committed vendor JSON. Lifecycle,
usage, and error adapters import those generated definitions; they do not own
handwritten copies. The committed
[`CodexErrorInfo.json`](src/schemas/codex/CodexErrorInfo.json) is the vendor-owned
error leaf, including its structured payloads and open future-value branch.
[`provider_errors.rs`](src/provider_errors.rs) consumes its generated known tags.
`studio-contracts` renders
[`provider-errors.generated.ts`](../../../providers/apps/claude-bridge/provider-errors.generated.ts).
The [assistant error adapter](../../../providers/apps/claude-bridge/features.ts)
uses the SDK's error type directly with an exhaustive mapping and checks runtime
tags before indexing it. Unknown or ambiguous errors remain unclassified with
their raw evidence intact; this is not full error-payload validation.

The [root manifest](../../../../package.json) owns the vendor commands:

- `pnpm run contracts:vendor:generate` regenerates consumers from committed sources.
- `pnpm run contracts:vendor:check` checks generated output and provenance offline.
- `pnpm run contracts:vendor:refresh` extracts the selected sources from the pinned CLI.
- `pnpm run contracts:vendor:verify-source` compares the committed sources with
  fresh extraction from that CLI and refuses an absent or mismatched executable.

Cargo checks generated freshness before compiling the schema crate. Normal
offline builds require Node and rustfmt, but do not need installed JavaScript
packages, the provider CLI, or a model request. The Claude rate projection records
committed source and artifact fingerprints; the full vendor check also verifies
the installed SDK declaration digest. Provider upgrades
must also run source verification; offline freshness cannot inspect an unrelated
installed executable. Mutation tests alter consumed vendor fields and variants
and require actual production consumers to fail compilation.

[`provider_input/items.rs`](src/provider_input/items.rs) owns the six fresh
Claude bridge item variants: user message, agent message, command execution,
file change, MCP tool call, and context compaction. `studio-contracts` derives
their serialized envelope in
[`runtime-contract.generated.ts`](../../../providers/apps/claude-bridge/runtime-contract.generated.ts).
The [bridge](../../../providers/apps/claude-bridge/bridge.ts) uses that contract
for new item emission, preserving true omission of absent native fields rather
than inventing nulls or strings. Tool payloads outside the explicitly validated
contracts below, results, and compaction metadata stay opaque. A separate historical emitter handles only retained old
records that cannot be losslessly converted into fresh required fields; it does
not invent identities. This output contract is not an ingress validator.

[`provider_lifecycle.rs`](src/provider_lifecycle.rs) defines shared terminal
outcomes and read-only thread/turn observations. Selected vendor status leaves
come from the same pinned source and unified generator through
[`lifecycle.schema.json`](src/schemas/codex/lifecycle.schema.json) and the bridge's
[`codex-generated`](../../../providers/apps/claude-bridge/codex-generated) modules.
The contract renderer adds `TerminalTurn`, `TerminalTurnStatus`, and
`ThreadIdentity` to the bridge's generated runtime contract. The
[SDK result adapter](../../../providers/apps/claude-bridge/provider-lifecycle.ts)
maps the SDK result lifecycle fields and validates those fields at ingress.
The bridge applies it before terminal finalization; an unrecognized result
disarms terminal timers and leaves the turn unconfirmed for reconciliation.
The runtime [scheduler](../runtime-services/src/scheduler.rs),
[recovery](../runtime-services/src/recovery.rs), and
[restart recovery](../runtime-services/src/account_worker/restart_recovery.rs)
consume the typed observations. Unknown or malformed lifecycle evidence cannot
prove a terminal outcome or idle thread; retained receipts, raw history, and the
distinction between Studio thread and native session identity remain intact.
These projections do not validate a complete SDK result or rewrite stored thread
and turn records.

Claude types come directly from its pinned SDK. Runtime parsers must construct
values checked against the SDK projection; a trusted TypeScript predicate alone
does not prove that its checks track a changed SDK field. Intentionally open
native history remains validated opaque evidence where the SDK publishes no
closed shape.

The bridge's [`question-input.ts`](../../../providers/apps/claude-bridge/question-input.ts)
validates fresh question fields and tuple cardinalities against the SDK type,
including the synthetic dialog caller. Its
[`rate-limit-input.ts`](../../../providers/apps/claude-bridge/rate-limit-input.ts)
owns the SDK-derived rate projection. The unified provider generator emits its
JSON schema, consumed by [`provider_rate_limits.rs`](src/provider_rate_limits.rs)
before resume or compaction decisions. Unsupported evidence retains its raw
payload in a generated envelope and cannot establish a fresh, allowed limit.

Two known declaration inconsistencies are isolated in
[`sdk-contract-gaps.ts`](../../../providers/apps/claude-bridge/sdk-contract-gaps.ts)
using exact vendor input/output types and narrowly described expected errors.
The matching compiler witnesses require these directives to fail as unused when
the vendor repairs its declarations. This exception does not authorize casts or
parallel vendor types elsewhere.
