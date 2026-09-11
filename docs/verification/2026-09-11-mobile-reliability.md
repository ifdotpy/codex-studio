# Mobile reliability, 2026-09-11

## Scope

Source baseline: `5851a3f6f607836ced8348a211023a4c51460ded`.
Task branch: `fix/studio-mobile-reliability`.
Production renderer build: `c49e52afae84bce8`.

The mobile client must open saved chats, retain drafts, and resume safe message
retries after a connection loss. SQLite remains authoritative on the Mac.
Workspace verification remains necessary before cached drafts can leave the device.

## Changes

- Chat sync excludes work result histories before the database read.
  The work API retains the full records. The default state API remains compatible.
- Startup uses the sync projection and a small session request.
  It no longer downloads two full state documents at the same time.
- Projections and drafts share one event stream and one fallback clock.
  Hidden pages close that stream. A return replaces the old socket.
- A saved workspace opens after a 500 ms identity grace.
  An unresolved identity permits cached display only.
- Read deadlines expire after a browser suspension through a wall-clock check.
  Ordinary mutations do not acquire an automatic retry or a new deadline.
- The composer unlocks after the outbox stores the message.
  Each chat preserves order. Four chats can drain independently.
- Retries retain the exact message ID and body across tabs and reloads.
  Old replies cannot replace confirmed delivery or restore an older draft.
- The service worker caches exact public static assets.
  A complete newer cache can serve its own HTML, JavaScript, and CSS.
  A failed install preserves the previous complete cache.
  API responses and external resources remain outside that cache.

## Measurements

The production fixture contains 700 work records with generated text.
It contains no user history. Chromium uses a 390 by 844 viewport.
Network limits: 200,000 bytes/s download, 100,000 bytes/s upload, 100 ms latency.
The CPU rate multiplier is four.

| Measurement | Result |
| --- | ---: |
| Full state, gzip level 3 | 4,369,810 bytes |
| Chat state, gzip level 3 | 15,034 bytes |
| Cold composer | 3,765 ms |
| Cached draft after reload | 323 ms |
| Shared sync streams | 1 |

[Machine-readable measurements](2026-09-11-mobile-reliability.json) identify the build and fixture.
The separate saved-workspace check held the identity request while the browser stayed online.
Cached state appeared in 538 ms in Chromium and 545 ms in WebKit 2359.
Both checks blocked reads and draft writes to a different workspace.

WebKit caches can require a second static download during the first offline preparation.
One measured run transferred 999,078 bytes, including the initial 521,800-byte gzip shell.
That WebKit run opened the cached shell in 1,270 ms while the Mac connection failed.
WebKit timing here is unthrottled and does not represent physical iPhone performance.

## Checks

Passed: TypeScript, production renderer and panel builds.
Passed: mobile state contract (7), critical sync contract (7), sync contract, and sync HTTP contract.
Passed in Chromium and WebKit: mobile sync, cached identity gate, request deadlines,
message lifecycle, full App delivery, and service worker lifecycle.
The HTTP delivery check used the real server and SQLite.
Two tabs retried after a lost reply and reload. SQLite retained one event and one history row.

Existing message delivery checks retain node identity, focus, position, and height assertions.
The state fixture now accepts the advertised `state:chat` scope.
Passed: mobile layout, mobile keyboard geometry, message delivery, snapshot order,
workspace, and outbox controls.
Mobile fixtures use the search button, account picker, project account, and desktop
terminal controls introduced in `e4ef04a`.
Mobile layout checks retain the quota footer added in that commit.
Keyboard checks account for its actual bounds and CSS spacing.
They still reject overlap, viewport overflow, and unexplained space below the controls.

Reproduce the new browser checks with `npm --prefix web run test:mobile`.
Use `BROWSER=webkit` for WebKit. The request deadline test runs both engines.
The performance fixture always uses Chromium because it requires its network controls.
Python 3.11 or later must be available as `python3` on PATH.

## Package

The package and its hidden startup check passed.
The check used a separate state directory and stopped only its fixture backend.
The package contains the exact renderer and backend sources listed above.

Artifact: `desktop/dist/Codex Studio-darwin-arm64/Codex Studio.app`.
Tree SHA-256: `d7bcffc1d634a7e3b409a3fee944be4d2cc66e4efed16d6dd09a261687a4def6`.
Renderer index SHA-256: `55456e7af0a9563eb0ffc9827f327f2b52ed5f51b72280ef64e49b999febeb53`.
Package check evidence: `codex-desktop-package-test-IBZUAZ` in the system temporary directory.

## Delivery limits

The installed backend remains unchanged while user agents run.
The repository rule forbids stopping active agents, monitors, or terminals for an update.
The new renderer and backend must be installed together after that work finishes.
No physical iPhone, mobile carrier, microphone, live model request, or background iOS delivery is certified here.
