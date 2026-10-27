# Type contracts

This page owns the cross-language typing policy for maintainers changing Studio
contracts. The required architecture below guides new and migrated code;
[current coverage](#current-migration-coverage) records where the source still
falls short. Provider-specific rules belong to the
[provider extensibility contract](../workspaces/providers/README.md#provider-extensibility).
Language-specific rules belong in [TypeScript](typescript.md), and check
selection belongs in [Testing](testing.md).

## One owner per contract

Every shared type, domain value, and constant has one named owner. Its owner
defines its meaning, representation, and unit. Consumers import that definition
or use generated output. Equal values with different purposes or units remain
distinct contracts. Validators must use the same owner. Where generation is
unavailable, a fast parity check must name the owner and cover every consumer.

Choose the owner at the boundary that defines the contract:

- **External library or protocol:** Use published SDK types or generated
  protocol schemas. Pin dependencies in their manifests. Do not copy an external
  shape into an optional-field object or maintain a second list of variants.
- **Studio domain:** Give each shared concept one owning contract. Use explicit
  variants for genuinely different data. Follow the provider extensibility
  contract for provider identity, adapters, and dispatch.
- **Studio wire and storage:** Use the owning Rust contract or committed schema,
  with generated declarations for other languages. The
  [runtime schema guide](../workspaces/runtime/packages/runtime-schema/README.md)
  identifies those sources and the contract renderer. Committed OpenAPI and
  persisted-state schema sources remain authoritative where a Rust definition
  has not replaced them through generation and parity checks.
- **Frontend:** Reuse the backend-owned contract for every applicable type,
  structure, field, state, and enumeration. Import generated types and runtime
  enum values; do not recreate response shapes, status lists, or wire literals
  in the frontend. Derive narrower views with indexed access, `Pick`, `Omit`,
  or `Extract` from that owner. If a shared concept lacks a backend contract,
  define it at that boundary and generate its consumers instead of inventing a
  frontend substitute.
- **Local implementation:** Keep genuinely local types and helpers with their
  owning module, such as focus, expansion, or draft presentation state that has
  no backend counterpart. Tests and fixtures consume the same owner.

Generated files are consumers. Change the owner and regenerate them through the
[root manifest](../package.json); do not repair drift by editing generated
declarations or adding a parallel handwritten schema.

## Validate at ingress, use types inside

Deserialize untrusted HTTP, provider, file, and stored data at its boundary into
a validated type before domain decisions. Deserialization must reject invalid
known fields and combinations. A declared type or cast alone does not validate
runtime input. Keep intentionally open JSON payloads at named extension or
opaque transport boundaries; interpret their fields through the owning adapter.

The renderer consumes generated types for first-party HTTP and SSE data. Do not
add a second response-schema validator on the client. Keep server ingress
validation and client checks for request identity, epoch, freshness, and operation
outcome with their existing owners. Local-storage hydration remains a separate
boundary; its existing partial guards do not prove arbitrary cached JSON valid.

Preserve resource-to-payload types through subscriptions and publish shared
entity read models as readonly values. Forms copy the fields they edit into
local drafts; partial local caches keep their own narrower contract instead of
claiming to be complete server responses.

Use a closed enum or discriminated union for a closed domain. Each variant
carries its required fields, so a successful result cannot lack its payload and
a command cannot lack its required arguments. Do not widen a closed domain to
`string`, copy an optional-field bag, or introduce an arbitrary unknown variant
to silence a compiler error. An external extension may be open only when its
owner defines how it is preserved and consumed.

Represent failure with `Result` or the language's typed error result. Prefer
required, non-null fields. Use `Option`, nullable values, or optional fields
only when absence is a real domain state; use one representation of absence
unless missing and explicit null have different defined meanings. Preserve
those distinctions when an existing wire contract requires them; remove
unnecessary nullability at the owner through the normal compatibility process,
not with consumer assertions or defaults. Do not turn malformed data, failed
reads, or failed operations
into an empty list, a default value, or successful absence. Unknown delivery or
recovery evidence remains an explicit uncertain outcome.

Use optional chaining only for absence that the owning type deliberately
permits and whose result the caller handles. Access required fields directly.
Do not use `?.`, `??`, or fallback values to make an unchecked or invalid
structure appear usable; validate it at ingress or narrow its variant first.

Convert domain values to storage or wire strings at the boundary through their
owner's `Display`, `toString`, serde codec, or equivalent conversion. Parse them
through that owner on entry. Internal dispatch uses the typed value, rather than
repeated string comparisons.

Handle every variant explicitly. Rust matches must expose new enum variants to
the compiler; TypeScript switches use a `never` proof and complete lookup tables
use `satisfies Record<Domain, Value>`. Do not add wildcard success paths, casts,
or fallbacks to make an incomplete dispatch compile.

Use the strictest applicable compiler and lint checks. Every statically
detectable contract violation must fail compilation, lint, or generated-contract
checks, including fixtures and scripts. Add compile-time witnesses for contract
constraints and exhaustive variants where the compiler needs help. Do not weaken
types, make fields optional, or suppress diagnostics to accommodate a mismatch;
correct the owning contract or its consumer. Runtime validation remains required
for untrusted data and invariants that static checks cannot establish. The
[TypeScript rules](typescript.md) own the language-specific prohibitions and
compiler configuration.

Prefer named enum variants when a boolean represents a domain outcome, phase,
or authority to act. A checked negative receipt, for example, carries the
identity it proves; a bare `confirmed` flag does not. Keep ordinary predicates
and SDK-defined boolean fields as booleans. Do not create a second representation
of a vendor contract merely to replace its boolean spelling.

## Migrate Studio-owned persisted formats

The [runtime migration policy](runtime-migrations.md) owns the dated migration
lifecycle, startup barrier, per-step rollback guarantee, and removal rules.
Provider identity semantics remain in the
[provider extensibility contract](../workspaces/providers/README.md#provider-extensibility).
Keep each current format's codec and validator with its named owner; centralize
historical server conversions through the migration contract instead of retaining
old-format branches in ordinary readers.

Conversions must be lossless: preserve historical meaning, exact request and
receipt identities, and unsettled or uncertain outcomes, without replaying an
operation or inventing completion evidence. Unknown future external inputs are
distinct from old owned formats; handle them at their declared extension or
unsupported boundary and retain recovery evidence. A Studio storage migration
does not convert external vendor-owned history. These requirements do not imply
that every existing format has already been migrated.

## Enforcement owners

| Concern                                           | Authoritative source                                                                                                                                                                              |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript compiler settings                      | [Shared compiler configuration](../tsconfig.base.json); every project extends it.                                                                                                                 |
| TypeScript restrictions and JavaScript exceptions | [TypeScript guide](typescript.md) and its linked exception owner.                                                                                                                                 |
| Rust lint levels and crate inheritance            | [Workspace manifest](../Cargo.toml) and member manifests.                                                                                                                                         |
| Shared Clippy configuration                       | [Root configuration](../clippy.toml); nested configurations retain its values.                                                                                                                    |
| Compiler inheritance and gate composition checks  | [TypeScript witness](../workspaces/tooling/apps/repository-checks/tests/compiler-policy.test.ts) and [Rust witness](../workspaces/tooling/apps/repository-checks/tests/rust-lint-policy.test.ts). |
| Schema generation and consumer parity             | [Contract renderer](../workspaces/runtime/packages/runtime-schema/src/contracts.rs).                                                                                                              |
| Lint rules and check selection                    | [Root lint configuration](../.oxlintrc.json), [root manifest](../package.json), and [Testing](testing.md).                                                                                        |

Compiler and parity checks enforce their declared scope. They do not prove that
every JSON consumer uses a domain type, that upstream runtime data is valid, or
that an external operation succeeded. Boundary tests must cover malformed input
and the real caller's success and failure paths.

The compiler-policy witnesses run the pinned Rust compiler, Clippy, and Oxlint
against small invalid programs. They check exhaustive enum handling, new required
fields, redundant fully bound struct patterns, and prohibited TypeScript escape
hatches. Caller signature tests and generated-contract checks cover the migrated
boundaries below. These checks do not ban `Value` at opaque payload boundaries.

[The enum checker](../workspaces/tooling/apps/repository-checks/enum-literals.ts)
uses the TypeScript compiler to resolve exact generated owners in tracked web
source and tests. It checks common comparisons, switch cases, call arguments,
and contextual field/variable initializers. Both staged checks and `lint:all`
run it; the fast, comprehensive and hosted non-Rust gates include the same
check. Staged workspace dependencies resolve from the index snapshot. Complex
flow/alias contexts, untyped strings and other projects remain outside its
coverage. Compiler strict subflags are checked individually. The Clippy
`disallowed_types` restriction on `serde_json::Value` is explicitly enabled
only for the pure ProviderKind parser; schema trees and opaque adapters remain
permitted. Schema doctests and the migrated service compile-fail witnesses run
in the local gates.

The same checker compares handwritten web type declarations with tracked
generated contract names and explicit string-literal union member sets. It
also checks defaults on string- or number-literal union property switches in web and
tooling sources. It accepts a terminating return or throw, or a call whose
argument and corresponding parameter are both `never`, or a `never`-typed
binding of the narrowed value. It does not check switches without a default,
prove all other branches are covered, detect structural copies of object types,
or follow `JSON.parse` results through typed data flow. Existing findings are tracked by the
[ratchet baseline](../workspaces/tooling/apps/repository-checks/ratchet-baseline.json).
After fixing a finding, remove its file and fingerprint from that baseline;
the full check rejects both new findings and entries whose finding disappeared.
Staged checks apply the same rule to staged source files and the staged baseline.

## Current migration coverage

The source contains typed contracts alongside legacy and open representations.
These examples identify coverage and remaining gaps; they are not a claim of a
completed repository-wide migration. The [2026-10-27 source census](audits/type-contracts-2026-10-27.md)
records the complete audited family inventory, inspection depth, exclusions,
remaining candidates and required compatibility evidence at its pinned revision.

| Boundary                    | Implemented ownership and remaining limit                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| HTTP and persistence        | [HTTP validation](../workspaces/runtime/packages/runtime-http/src/validation.rs) uses registered schemas; [extractors](../workspaces/runtime/packages/runtime-http/src/routes/extractors.rs) can deserialize typed bodies. [Stored records and tables](../workspaces/runtime/packages/runtime-store/src/records.rs) provide named IDs and schema checks, while some record wrappers and service operations still carry JSON values. Schema validation alone does not make their internal field access statically typed.                                                                                                                    |
| Shared generated contracts  | [Runtime schema](../workspaces/runtime/packages/runtime-schema/README.md) generates selected shared values, API declarations, usage observations, and bridge extensions. This does not mean that all HTTP or provider payloads originate in Rust structs.                                                                                                                                                                                                                                                                                                                                                                                  |
| Provider transport          | [Runtime providers](../workspaces/runtime/packages/runtime-providers/README.md) owns typed frame envelopes, methods, request IDs, and request lifecycle outcomes. Request parameters and response payloads remain open JSON at the codec boundary.                                                                                                                                                                                                                                                                                                                                                                                         |
| Provider identity           | [ProviderKind](../workspaces/runtime/packages/runtime-schema/src/provider_kind.rs) is the shared identity owner for account, agent, and usage consumers. The [renderer](../workspaces/runtime/packages/runtime-schema/src/contracts.rs) generates [TypeScript identity](../workspaces/client/apps/web/src/generated/providerKind.ts) and schema references. The [2026-10-26 startup migration](../workspaces/runtime/packages/runtime-store/src/migrations/m2026_10_26_provider_identity.rs) converts historical owned identities before strict current readers run; those readers reject absent, null, unknown, and malformed identities. |
| Vendor and bridge contracts | The [bridge manifest](../workspaces/providers/apps/claude-bridge/package.json) pins the SDK; bridge protocol and usage code consume its types. The [schema guide](../workspaces/runtime/packages/runtime-schema/README.md#provider-contract-sources) identifies the selected generated vendor leaves and Studio-owned envelopes. These cover selected boundaries, rather than a complete vendor protocol or every runtime payload.                                                                                                                                                                                                         |
| Provider errors             | [Known error classification](../workspaces/runtime/packages/runtime-schema/src/provider_errors.rs) and the [SDK assistant adapter](../workspaces/providers/apps/claude-bridge/features.ts) share generated known tags with an ingress guard. The vendor error leaf remains open; unknown error records retain their original payload. Classification does not validate every error field.                                                                                                                                                                                                                                                  |
| Fresh bridge items          | [FreshItem](../workspaces/runtime/packages/runtime-schema/src/provider_input/items.rs) generates a closed fresh-output envelope. [Bridge emitters](../workspaces/providers/apps/claude-bridge/bridge.ts) distinguish new items from retained historical records. Native payloads remain opaque, and static output types do not validate arbitrary input.                                                                                                                                                                                                                                                                                   |
| Turn and thread lifecycle   | [Lifecycle projections](../workspaces/runtime/packages/runtime-schema/src/provider_lifecycle.rs) and the [SDK result adapter](../workspaces/providers/apps/claude-bridge/provider-lifecycle.ts) supply typed observations to completion and recovery callers. Unknown lifecycle evidence supplies no terminal decision; raw stored records and native session identities remain separate.                                                                                                                                                                                                                                                  |

The following flows preserve their owning types through internal callers:

| Flow                              | Owner and boundary                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| --------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Work commands and reads           | [TaskCommand](../workspaces/runtime/packages/runtime-schema/src/task_command.rs) owns action variants for HTTP and managed tools. Owner-derived projections preserve historical Unicode code-point limits: title 160 for both, HTTP description/reason 64000/12000, and tool description/reason 32000/32000. Field annotations preserve native consumed-null refusals; unused legacy fields and raw signatures retain their original forms. Native dispatch and authenticated managed-tool ingress use the tool projection. HTTP schemas retain those variants; the SDK requires a generated flat tool advertisement without omission defaults. Server parsing enforces action-specific requirements, and the real SDK conversion has a generated-contract witness. [WorkItemRecord](../workspaces/runtime/packages/runtime-store/src/records.rs) stays typed through list and dependency policies; its original JSON preserves missing, null, and extension fields. Request signatures still use the original transport body; task results and result schemas remain opaque. |
| Claude mutations                  | [ClaudeControlCommand and ClaudeSettings](../workspaces/runtime/packages/runtime-schema/src/claude_controls.rs) own HTTP mutation arguments. [ControlInvocation](../workspaces/runtime/packages/runtime-services/src/claude_controls/service.rs) carries each retained method, its arguments and required request/turn identities; known native state, proof and mutation replies decode once after receipt validation. Every owned setting must match by presence and value, including omitted versus null fields; unknown extensions remain opaque. Malformed or contradictory replies remain uncertain and cannot repeat a write. Saved historical records and forwarded SDK payloads retain named JSON boundaries.                                                                                                                                                                                                                                                                                                                                                        |
| Model catalog                     | [ProviderModel and ModelCatalog](../workspaces/runtime/packages/runtime-schema/src/model_catalog.rs) own consumed fields. Provider ingress validates them before caching, admission, HTTP/SSE publication, and generated bridge/web consumers. Unknown metadata and upstream model, effort, and tier names remain open; pagination and account binding remain service policy.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| MCP reconnect and monitor input   | [Reconnect](../workspaces/runtime/packages/runtime-http/src/routes/mcp_reconnect.rs) preserves generated request/session types through execution, serializing at receipt-signature and provider boundaries. [Monitor adapters](../workspaces/runtime/packages/runtime-http/src/routes/monitor_runtime.rs) pass the existing runtime `Input` type; missing dimensions retain their write-versus-resize meaning. Ingress bounds and non-HTTP validation remain in their owners.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Transfer confirmation             | [ExecutionSettings](../workspaces/client/apps/web/src/components/agents/ExecutionSettings.tsx) keeps generated response and receipt types. Runtime status values derive from the OpenAPI status owner; notice provider identity uses `ProviderKind`. Confirmation retains runtime JSON and identity checks without claiming full response-schema validation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Receipt recovery                  | [OperationState](../workspaces/runtime/packages/operations/src/lib.rs) owns lifecycle meaning. [The receipt adapter](../workspaces/runtime/packages/runtime-services/src/tool_requests/lifecycle.rs) classifies stored stage/outcome pairs once, preserving historical `completed/not_applied` as `NotApplied` without rewriting storage. Unknown or invalid pairs cannot prove completion; cancellation requires matching identity, owner epoch, and negative finish evidence.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Task settlement                   | [Settlement readers](../workspaces/runtime/packages/runtime-services/src/task_settlement.rs) reuse checked store decoders with selected row IDs. Malformed records fail the transaction instead of disappearing as successful absence. Stored payloads remain schema-checked JSON.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Subscription publication          | [Subscription contracts](../workspaces/client/apps/web/src/sync/subscriptions/contracts.ts) retain resource and persisted-scope correlation through selectors, observers, and recovery. Two centralized assertions dispatch heterogeneous function maps. RxDB entities and Agent read models remain readonly; forms copy edit arrays. Work selectors reconstruct their existing list view. The local TranscriptProjectionPayload permits partial server metadata, while HTTP TranscriptPageResponse stays complete. Stream identity, epoch, revision, baseline, and freshness checks remain runtime policy.                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Retained account-session receipts | [SessionOutcome](../workspaces/runtime/packages/runtime-services/src/account_session_runtime.rs) owns written, worker-not-sent, uncertain, and specific or generic pre-handoff outcomes. Written requires a ticket and response; worker-not-sent and uncertain outcomes require tickets. NotHandedOff records generic ticketless non-delivery. Derived legacy diagnostic tags are not ticket evidence. Native, voice, catalog, safety, transfer, and HTTP callers retain scope, operation, generation, replay, permission, and current-owner checks. Wire/storage projections and opaque provider payloads remain unchanged.                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Process controls                  | [Process command and result contracts](../workspaces/runtime/packages/runtime-schema/src/process_controls.rs) own action-specific commands, status-specific outcomes, and found receipts carrying a checked result. HTTP and service callers retain original request JSON for signatures and validate stored result fields before publishing the original payload, preserving historical nulls, omission, numbers, and extensions. Generated web callers retain absence handling for legacy omitted process arrays. Epoch, generation, capabilities, native binding, and pagination freshness remain runtime policy.                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Usage values                      | [UsageTokens](../workspaces/runtime/packages/runtime-schema/src/usage.rs) checks the one-hour cache-write subset; [UsageLedgerRow](../workspaces/runtime/packages/runtime-schema/src/usage/ledger.rs) owns the immutable identity/evidence pair. Checked constructors and deserialization reject incoherent independent inputs. Analytics, reconciliation, and budget callers use immutable accessors; internally derived pairs are constructed once. Legacy receipt aliases and null/omitted receipts remain supported. External agent/receipt/token anchors, arithmetic overflow, replay, and transaction checks remain separate obligations; these types prove neither a charge nor durability.                                                                                                                                                                                                                                                                                                                                                                            |
| Account limits                    | [The workspace usage-limits schema](../workspaces/runtime/packages/runtime-schema/src/schemas/workspace-usage-limits.schema.json) owns consumed account data, buckets, windows, and eligibility. The renderer generates canonical OpenAPI leaves and legacy DTO aliases for HTTP, streamed entities, bridge producers, and web recovery. LimitsSnapshot remains checked through persistence and HTTP handoff; notifications are checked after merging. Provider extensions and absent/null fields survive. Recovery retains account, agent, model, provider, expiry, and freshness decisions. Durable cache hydration retains its existing partial guard and serialized-contract adapter; reset-credit identity and exact-once effects remain with their existing owner.                                                                                                                                                                                                                                                                                                      |
| Vault jobs                        | [Committed OpenAPI](../workspaces/runtime/packages/runtime-http/openapi.json) owns login/setup/pull response variants, required terminal outcomes, and correlated progress resource/value pairs. Three concrete web wrappers infer body and result from literal API paths. HTTP recovery reuses the original request identity; SSE completion checks resource identity and job. Success producers construct generated terminal patches. Pull command output is decoded after schema-owned numeric limits are checked; JSON number aliases preserve integral decimal representations. Static client contracts do not validate arbitrary response JSON or prove live vault execution.                                                                                                                                                                                                                                                                                                                                                                                           |
| Question and approval answers     | [QuestionCardAnswer and QuestionDecision](../workspaces/runtime/packages/runtime-schema/src/question_answers.rs) own fresh method-specific construction and native decisions. Automatic approvals and AnswerForm consume the generated owner. The async writer normalizes string leaves once from the current stored question for follow-up text and redacted history; independently supplied text is no longer accepted. Raw native extensions, historical empty-answer forms, omitted/null signatures, pending/epoch/ownership checks, and uncertain delivery retain their boundaries.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| TypeScript generation             | [The generator](../workspaces/tooling/apps/repository-checks/generate-api-types.ts) uses pinned openapi-typescript and TypeScript AST transformations. Every named closed string enum component emits a runtime const object and a type alias referencing its original schema property; equal-valued domains remain distinct owners. Normalized names, computed keys and collisions have generator/checker witnesses. Schemars definitions paths produce references directly, without recursive JSON rewriting or serialize/text-replace/reparse passes. Generated declarations do not validate runtime response data.                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Start waits                       | [AgentStartWait](../workspaces/runtime/packages/runtime-schema/src/agent_progress.rs) is an epoch-scoped tagged enum: slot waits require counts and pause waits require a deadline. Provider startup carries a retry deadline only when the registry has one. The generated entity DTO remains strict. The separate legacy status record accepts historical omitted/null details and extensions; incomplete current progress becomes unavailable without hiding the agent or inventing evidence, while stale epochs retain the existing queued fallback. Independent holds and admission identities remain authoritative. Schema bounds retain the shared JavaScript-safe integer limit; historical Rust integer decoding is unchanged.                                                                                                                                                                                                                                                                                                                                       |
| Transfer decisions                | [TransferDispatch and receipt reducers](../workspaces/runtime/packages/runtime-services/src/account_transfer.rs) retain SessionReceipt and ProviderResponse through fresh reads and mutations. Ticketless transport uncertainty is distinct from worker-proven non-delivery; pre-receipt refusals do not manufacture NotSent. Rejected replies cannot supply an adopted thread. Durable JSON, exact scope and replay guards remain unchanged. Stored replay still crosses a JSON boundary and parses RequestId in both authorization and decoding.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Managed dispatch and recovery     | [DispatchedOperation](../workspaces/runtime/packages/runtime-services/src/managed_host_tools.rs) owns applied, rejected, not-sent and unknown callback outcomes, each carrying its result and signals. Actual child creation producers return that outcome. [RecoveryReceipt](../workspaces/runtime/packages/runtime-services/src/recovery.rs) carries semantic confirmation through the account worker, retained port and server broker to callback settlement; RecoveryOutcome::Applied denotes a committed transaction and may contain an unknown receipt. Stored legacy status is interpreted only by the named saved-receipt adapter. Heterogeneous tool payloads stay opaque.                                                                                                                                                                                                                                                                                                                                                                                           |
| Server access                     | [The access client](../workspaces/client/apps/web/src/api.ts) derives requests and AccessSnapshot from the generated multi-server operation. GET and POST retain method-specific bodies and results; UI actions retain their narrower supported set, required settings and request identity. Scoped tokens, workspace headers, timeouts and SSE recovery remain runtime behavior.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Native draft publication          | [DraftSnapshot](../workspaces/client/packages/client-domain/src/drafts.rs) owns workspace identity, committed revision, error and reconciled draft fields. The store carries it to the native response serialization boundary, and the renderer uses its generated declaration. Published snapshots are deeply readonly at their TypeScript API; editing forms copy the values they change. This is a static API constraint, not a runtime freeze or proof that structurally compatible mutable aliases cannot exist. Local initial revision and journal/replay policy retain their separate meanings.                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |

When migrating a boundary, update its owner, adapters, generated consumers, and
caller tests together, following the [persisted-format migration policy](#migrate-studio-owned-persisted-formats)
for stored data. Update this coverage section when the cited source changes
ownership.
