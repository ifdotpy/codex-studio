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

## Current migration coverage

The source contains typed contracts alongside legacy and open representations.
These examples identify coverage and remaining gaps; they are not a claim of a
completed repository-wide migration.

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

When migrating a boundary, update its owner, adapters, generated consumers, and
caller tests together, following the [persisted-format migration policy](#migrate-studio-owned-persisted-formats)
for stored data. Update this coverage section when the cited source changes
ownership.
