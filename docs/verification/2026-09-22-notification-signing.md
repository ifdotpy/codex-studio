# Persistent desktop notification identity

The packaged application now requires a certificate signature. Previous builds
used an ad hoc signature whose designated requirement contained the build's
code hash. Saved macOS notification permission sources referenced older hashes.

The local installation uses a dedicated `Codex Studio Local Signing` certificate
and keychain. It does not use the CallScribe keys. The private configuration and
password stay outside the repository. The signer also supports an explicitly
selected certificate and keychain for other machines.

A notification permission failure now produces one clear message per renderer
session. Future alerts still call the native bridge so permission recovery does
not require resetting application state. Other failures retain their retry behavior.

Checks on 2026-09-22:

- The web production build passed.
- `node tests/desktop-notifications-ui.mjs` passed. It covers permission refusal,
  repeated snapshots, a later alert after permission recovery, and current-chat suppression.
- `node desktop/test.mjs` passed, including native failure mapping and backend survival.
- `npm run package` in `desktop/` passed with the dedicated certificate.
- `node desktop/package-test.mjs` passed against the signed package.
- A native probe bundle signed before and after a resource change retained the
  same certificate-based designated requirement. Both strict signature checks passed.

These checks do not prove that macOS displayed a notification banner. The open
application process must restart to load the changed native handler. A previous
permission grant may need renewal in macOS Notifications after this identity change.
The backend and active agents do not need a restart.
