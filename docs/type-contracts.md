# Type contracts

This page owns the cross-language typing policy for maintainers changing Studio
contracts. The required architecture below guides new and migrated code;
[current coverage](#current-migration-coverage) records where the source still
falls short. Language-specific rules belong in [TypeScript](typescript.md), and
check selection belongs in [Testing](testing.md).

## One owner per contract

Every shared type, domain value, and constant has one named owner. Its owner
defines its meaning, representation, and unit. Consumers import that definition
or use generated output. Equal values with different purposes or units remain
distinct contracts. Validators must use the same owner. Where generation is
unavailable, a fast parity check must name the owner and cover every consumer.

Choose the owner at the boundary that defines the contract:

- **Vendor protocol:** Use the provider's published SDK types or generated
  protocol schemas. Pin dependencies in their manifests. Do not copy a vendor
  shape into an optional-field object or maintain a second list of variants.
- **Studio domain:** Normalize provider data through an explicit adapter into
  one shared Studio contract. Provider identity and common thread, turn, item,
  and outcome semantics belong here. Keep genuinely provider-specific data in
  explicit variants; preserve the distinction between a native provider session
  and Studio's thread or mirror identity.
- **Studio wire and storage:** Use the owning Rust contract or committed schema,
  with generated declarations for other languages. The
  [runtime schema guide](../workspaces/runtime/packages/runtime-schema/README.md)
  identifies those sources and the contract renderer. Committed OpenAPI and
  persisted-state schema sources remain authoritative where a Rust definition
  has not replaced them through generation and parity checks.
- **Local implementation:** Keep types and helpers with their owning module.
  Tests and fixtures consume the same owner.

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

Represent failure with `Result` or the language's typed error result. Use
`Option`, nullable values, or optional fields only for meaningful absence;
preserve distinctions between missing and explicit null when the wire contract
requires them. Do not turn malformed data, failed reads, or failed operations
into an empty list, a default value, or successful absence. Unknown delivery or
recovery evidence remains an explicit uncertain outcome.

Convert domain values to storage or wire strings at the boundary through their
owner's `Display`, `toString`, serde codec, or equivalent conversion. Parse them
through that owner on entry. Internal dispatch uses the typed value, rather than
repeated string comparisons.

Handle every variant explicitly. Rust matches must expose new enum variants to
the compiler; TypeScript switches use a `never` proof and complete lookup tables
use `satisfies Record<Domain, Value>`. Do not add wildcard success paths, casts,
or fallbacks to make an incomplete dispatch compile.

## Migrate Studio-owned persisted formats

Prefer a dated, lossless one-time migration over permanent compatibility readers
for old Studio-owned formats. Use the format owner's existing version or
migration mechanism, then let current readers accept only the current format.
The [store guide](../workspaces/runtime/packages/runtime-store/README.md) identifies
existing feature migration markers; it does not provide a database-wide version
or a general migration runner.

Record the migration date and format or migration version, affected data, required
support floor (the oldest source format or application version still supported),
and the condition for removing the migration. Remove it only after supported
upgrade paths no longer need it; its date alone does not make deletion safe.
An older installation must have an explicit supported upgrade path or a visible
refusal. Preserve historical meaning, exact request and receipt identities, and
unsettled or uncertain outcomes; conversion must not replay an operation or
invent evidence of completion.

Unknown future vendor inputs are external protocol changes, not old Studio-owned
formats. Handle them at the vendor adapter's declared extension or unsupported
boundary, retaining the evidence needed for recovery. Do not coerce them into a
known variant or run a Studio storage migration against vendor-owned history.
These are migration requirements, not a claim that every existing format has
already been migrated.

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

| Boundary                    | Implemented ownership and remaining limit                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| HTTP and persistence        | [HTTP validation](../workspaces/runtime/packages/runtime-http/src/validation.rs) uses registered schemas; [extractors](../workspaces/runtime/packages/runtime-http/src/routes/extractors.rs) can deserialize typed bodies. [Stored records and tables](../workspaces/runtime/packages/runtime-store/src/records.rs) provide named IDs and schema checks, while some record wrappers and service operations still carry JSON values. Schema validation alone does not make their internal field access statically typed.                                                                                                                                                  |
| Shared generated contracts  | [Runtime schema](../workspaces/runtime/packages/runtime-schema/README.md) generates selected shared values, API declarations, usage observations, and bridge extensions. This does not mean that all HTTP or provider payloads originate in Rust structs.                                                                                                                                                                                                                                                                                                                                                                                                                |
| Provider transport          | [Runtime providers](../workspaces/runtime/packages/runtime-providers/README.md) owns typed frame envelopes, methods, request IDs, and request lifecycle outcomes. Request parameters and response payloads remain open JSON at the codec boundary.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Provider identity           | [ProviderKind](../workspaces/runtime/packages/runtime-providers/src/provider_kind.rs) owns account and agent provider identity and explicit legacy parsing. [UsageProvider](../workspaces/runtime/packages/runtime-schema/src/usage.rs) is still a separate representation. A shared provider owner across these consumers remains migration work.                                                                                                                                                                                                                                                                                                                       |
| Vendor and bridge contracts | The [bridge manifest](../workspaces/providers/apps/claude-bridge/package.json) pins the SDK; [bridge protocol](../workspaces/providers/apps/claude-bridge/bridge-protocol.ts) and [usage normalization](../workspaces/providers/apps/claude-bridge/usage.ts) import its types. [Studio bridge extensions](../workspaces/runtime/packages/runtime-schema/src/provider_input.rs) and usage observations generate their TypeScript consumers. The committed [Codex usage notification schema](../workspaces/runtime/packages/runtime-schema/src/schemas/codex/ThreadTokenUsageUpdatedNotification.json) covers that notification, rather than the complete vendor protocol. |
| Threads, turns, and items   | [Bridge session state](../workspaces/providers/apps/claude-bridge/session-state.ts) keeps permissive legacy disk records and stronger fresh/completed turn types. Fresh items still use a string kind and an optional-field record. A shared normalized domain with exhaustive provider-specific variants is not yet universal.                                                                                                                                                                                                                                                                                                                                          |

When migrating a boundary, update its owner, adapters, generated consumers, and
caller tests together, following the [persisted-format migration policy](#migrate-studio-owned-persisted-formats)
for stored data. Update this coverage section when the cited source changes
ownership.
