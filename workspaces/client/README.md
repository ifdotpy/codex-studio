# Client workspace

Owns Studio's renderer and desktop shell. The renderer presents application
state through the server HTTP API. The native host owns operating-system
privileges and embeds the web build.

## Apps and navigation

- [`apps/web/`](apps/web): React and TypeScript renderer; see its
  [developer guide](apps/web/README.md) and [`package.json`](apps/web/package.json).
- [`apps/desktop/`](apps/desktop): Electron host, native bridge, packaging,
  and desktop tests; see its [developer guide](apps/desktop/README.md) and
  [`package.json`](apps/desktop/package.json).

## Rust client migration

The target is Tauri + Rust on desktop and PWA + WebAssembly on mobile.
Mobile Tauri builds are experimental and are not a release target for now.
The native desktop client now replaces the installed Electron shell. Electron
remains available for profile export and owns the shared packaging helpers.
See the [native guide](apps/native/README.md) for migration and verification.

- `apps/native`: Tauri desktop host and its hidden WebView integration checks.
- `packages/client-domain`: shared Rust rules, Markdown trees and protocol types.
- `packages/client-core`: native storage, transport and operating-system adapters.
- `packages/client-wasm`: browser bindings for shared rules. Markdown currently
  uses this module in the PWA; browser persistence and network adapters remain.

Install `wasm32-unknown-unknown` with rustup and wasm-bindgen-cli 0.2.129 with
`cargo install wasm-bindgen-cli --version 0.2.129 --locked`. The web build generates
bindings from the locked Rust workspace. Generated bindings are not committed.
`pnpm --filter codex-agents-web run test:wasm` compares the compiled WASM parser
with the native parser, including Unicode, links, rich fences and unsafe markup.
The hashed WASM asset is included in the service worker's initial offline cache.

The web app uses the server HTTP contract and must not access server SQLite or
provider credentials directly. Local client data belongs to the platform's
storage adapter. `TODO(attar-sync)` marks local operations to make synchronous
when the Attar host can acknowledge the durable commit synchronously.

For one hypothesis, use `just test codex-agents-web <file-path-substring>`;
add `--name <pattern>` to select a named Vitest case within that file. For a
renderer change, use `just check codex-agents-web`. The root
[`justfile`](../../justfile) discovers package names from manifests. App
manifests and Vitest own test discovery and selection. The pre-PR server/client
gate is documented in [docs/testing.md](../../docs/testing.md).
