# Providers workspace

Owns external model-provider process integrations used by Studio.

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
