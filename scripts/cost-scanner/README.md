# Account cost scanner

This helper uses the native Codex log scanner from CodexBar `v0.37.2`.

- Upstream: <https://github.com/steipete/CodexBar/tree/v0.37.2>
- Source commit: `f380287041b82e44672d29e4e5a5aef8a1691acb`
- License: [LICENSE-CodexBar.txt](LICENSE-CodexBar.txt)

`codex-cost-scanner --home ABSOLUTE_HOME --cache ABSOLUTE_CACHE` reads only
`ABSOLUTE_HOME/sessions`, its sibling `archived_sessions`, and the profile's
`logs_2.sqlite` when present. It does not scan Pi sessions, credentials, or
network sources. The scanner entry point reads the cached pricing catalog only;
it never calls CodexBar's asynchronous pricing refresh. The cache root is
explicit and must be unique per profile.

The vendored files keep CodexBar's incremental cache, duplicate and fork
accounting, token parsing, and built-in pricing. When available, the helper
copies only CodexBar's non-secret `models-dev-v1.json` pricing catalog into
the selected cache root. It never copies the shared token usage cache.

The wrapper marks history coverage as partial until the caller verifies the
selected profile's complete history. Unknown model pricing stays unknown.
