# Claude provider bridge

## Change Contract

This app owns the Node process that connects Studio's provider callers to the
Claude Agent SDK. Its public boundary is the bridge process protocol used by
the server, desktop package, and Linux guest. Keep SDK session state and
credentials within their owning process; do not let the bridge access Studio's
SQLite database or renderer state. Protocol behavior and dependency versions
are owned by this app's source and [`package.json`](package.json). Focused check:
`pnpm run test:server:bridge` from the repository root, as defined by the
[root manifest](../../../../package.json).
