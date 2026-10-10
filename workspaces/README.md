# Workspaces

Studio groups source by runtime, provider, client, and repository tooling. Apps
are runnable products or services; shared packages will be added only when a
real caller and focused checks justify them.

## Domains

- [Runtime](runtime/README.md): `apps/server` and `apps/vm-guest`; Python server
  and guest service. Focused check: `pnpm run test:server:python -- --filter <name>`.
- [Providers](providers/README.md): `apps/claude-bridge`; Node provider bridge.
  Focused check: `pnpm --filter studio-claude-bridge run test`.
- [Client](client/README.md): `apps/web` and `apps/desktop`; React renderer and
  Electron host. Focused check: `pnpm --filter codex-agents-web run test:unit -- <pattern>`.
- [Tooling](tooling/README.md): `apps/repository-checks`; staged-content and
  repository checks. Focused check: `pnpm --workspace-root run test:pre-commit`.

Today there are no shared packages. Apps depend on provider and runtime
interfaces in the direction needed to assemble a user-facing product; packages
must never depend on apps. Each workspace README links to the app manifests and
focused checks that own its commands.
