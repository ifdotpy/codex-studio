# Client workspace

Owns Studio's renderer and desktop shell. The renderer presents application
state through the server HTTP API. Electron owns native privileges and embeds
the web build.

## Apps and navigation

- [`apps/web/`](apps/web): React and TypeScript renderer; see its
  [developer guide](apps/web/README.md) and [`package.json`](apps/web/package.json).
- [`apps/desktop/`](apps/desktop): Electron host, native bridge, packaging,
  and desktop tests; see its [developer guide](apps/desktop/README.md) and
  [`package.json`](apps/desktop/package.json).

There are no client packages yet. The desktop app assembles the web build and
runtime; the web app depends on the server's HTTP contract and must not access
SQLite or provider credentials directly.

Focused checks: `pnpm --filter codex-agents-web run test:unit -- <pattern>` and
`pnpm --filter codex-agents-desktop run test`. Each app manifest owns package
commands.
