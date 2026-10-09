# Tooling workspace

Owns repository-wide source checks and the staged-content checks used by the
pre-commit hook.

## Apps and navigation

- [`apps/repository-checks/`](apps/repository-checks): lint and formatting
  checks for staged Git content; see its [Change Contract](apps/repository-checks/README.md).
  The root [`package.json`](../../package.json) owns the commands and `.githooks/`
  invokes the checker.

There are no tooling packages yet. Repository checks may inspect all workspaces
but application code must not depend on this tooling app at runtime.

Focused check: `pnpm --workspace-root run test:pre-commit`; the root manifest
owns this command and the hook's fixture test.
