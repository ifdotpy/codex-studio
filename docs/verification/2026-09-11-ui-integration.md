# Studio UI integration, 2026-09-11

## Sources

The integration branch is `integrate/studio-ui`.
The remote baseline is `c74d9386fa568ed4a2eb450aecfc07611932c31b`.
The local main was `841f058d0557b8d6a5c2c6e2b0b53cb2e2385d78`.
The saved working tree is `b5963553a833ee3e3a993493c9b0d9d12a2a265b`.

The branch includes the accumulated renderer, desktop, and supporting server work.
It includes themes, Settings, Messages, team navigation, inline queues, account
controls, streaming text, dictation, and native error recovery.
The local main's four commits remain part of the integration history.

All other UI branch heads were already represented in the remote main, except
the installed queue backport and a patch-equivalent native error commit.
The backport's focused test and historical verification record are retained.
Its old renderer does not replace the current source.
Dirty team-chat and stale-turn worktrees matched commits already in main.
The three unrelated harness research files remain outside this integration.

## Corrections

- Stop cancels browser recovery before a later resume request can be submitted.
- Dictation Delete and Undo preserve an edited transcript.
- Remote images load after explicit consent under the production content policy.
  Raw SVG and background attributes cannot bypass that consent.
- Mobile task and safety components have distinct React keys.
- Long command output wraps in both current and historical tool cards.
- The HTTP server accepts bursts of asset requests with the platform backlog.
- Fresh quota from the same account clears the current limit warning without a send.
  The result survives chat remounts and later failed reads for that error episode.
  A new error remains visible. Historical errors and retry permissions do not change.

## Checks

The broad run executed 37 backend commands and 77 web commands.
The first run had one backend failure and 15 web failures.
The relevant product fixes and obsolete fixture corrections were then checked again.
Updated fixtures follow the current queue button, Settings, Messages, project-file
access, and tool-card contracts. They retain delivery and security assertions.

Targeted checks cover browser recovery, dictation, external-image consent,
rich-preview isolation, native errors, quota recovery, queue delivery, live text,
large history, navigation, account transfer, and terminal lifecycle.
The terminal check uses an isolated real shell and verifies no agent wake.
The quota check uses real React components with controlled account responses.
It preserves history and sends no model request.

The TypeScript and production renderer build passed.
Desktop checks passed: seven backend tests and 20 hidden Electron cases.
The packaged application passed its isolated startup and bridge check.
It starts its bundled backend and retains that backend after its window closes.
These checks do not stop or modify the user's active Studio backend.

Local logs and initial result manifests:
`/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-ui-integration-g9wrpc7r`.
Desktop evidence: `codex-desktop-test-WllhNN` in the same temporary parent.
Package evidence: `codex-desktop-package-test-wIsIxz` in that parent.

## Artifact

Package: `desktop/dist/Codex Studio-darwin-arm64/Codex Studio.app`.
SHA-256 identities:

| Input | SHA-256 |
| --- | --- |
| Renderer index | `ac1fa37ca3425f56791bbc9b9624db7142fe31d6373346a7ff0aef7542026134` |
| Desktop app.asar | `fcf32873c508bc787cffbcebc78cd3b4dd7995d66ebfff05492e4a9068b12c18` |
| Package executable | `d3762bdff983315f18c07c3754175c4aa5b49d80977e1c33eb226b8189b447ed` |

The installed application still has an older renderer and backend.
Replacing it while its backend runs would mix old Python code with new files.
Installation therefore requires the existing active work to finish first.
No live model call, microphone recording, or credit redemption is claimed.
