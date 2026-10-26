# Providers workspace

Owns external model-provider process integrations used by Studio.

See [Type contracts](../../docs/type-contracts.md) for cross-language typing
rules and current migration coverage.

## Provider extensibility

Shared application code works with provider-neutral contracts. Provider identity
has one Rust owner, [`ProviderKind`](../runtime/packages/runtime-schema/src/provider_kind.rs).
Rust consumers import that enum; TypeScript and schema consumers use its generated
contract. Do not maintain a second provider enum, union, or list in a consumer.

Parse external provider names at the boundary. Missing, unknown, or malformed
names must remain unsupported, never select an existing provider by default.
Convert historical representations in a dated storage migration before normal
readers run; do not retain compatibility defaults in the identity owner or callers.
Each migration records its introduction date and the oldest supported upgrade
source. Remove it only when supported installations and restorable backups have
crossed that format boundary; elapsed time alone does not establish this.

Provider dispatch must be exhaustive: use an enum `match` or a typed mapping that
requires every supported provider. Do not use `if provider == "codex" ... else ...`,
a wildcard arm, or a fallback that gives a new provider another provider's behavior.
Keep native SDK, credentials, transcript formats, and provider-specific semantics
in their owning adapters. Shared callers consume normalized values or the
adapter's explicit capabilities.

To add a provider, extend the identity owner and its adapter, regenerate contracts,
and implement the cases exposed by exhaustive checks. Verify the new boundary
alongside existing providers and unknown input. Reuse the existing adapter and
generation mechanisms; provider support does not require a new framework.

## Apps and navigation

- [`apps/claude-bridge/`](apps/claude-bridge): the Node Claude Agent SDK
  process; see its [Change Contract](apps/claude-bridge/README.md) and
  [`package.json`](apps/claude-bridge/package.json).

There are no provider packages yet. Runtime and desktop apps call the bridge;
the bridge does not import either app. Keep provider credentials and native
session behavior with the provider integration that owns them.

For one hypothesis, use `just test studio-claude-bridge [case]`; for a bridge
change, use `just check studio-claude-bridge`. The root
[`justfile`](../../justfile) discovers packages from manifests; the bridge
manifest owns test execution and discovery. The pre-PR server/client gate is
documented in [docs/testing.md](../../docs/testing.md).
