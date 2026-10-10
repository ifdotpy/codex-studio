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

For one hypothesis, use `just test codex-agents-web <file-path-substring>`;
add `--name <pattern>` to select a named Vitest case within that file. For a
renderer change, use `just check codex-agents-web`. The root
[`justfile`](../../justfile) discovers package names from manifests. App
manifests and Vitest own test discovery and selection. The pre-PR server/client
gate is documented in [docs/testing.md](../../docs/testing.md).
