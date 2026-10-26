# Native desktop client

This Tauri host is the native desktop client. The installed macOS client was
migrated from Electron with its profile preserved on 2026-10-16. See the
[desktop parity report](../../../../docs/desktop/native-parity-20261016.md) for
verified behavior and remaining limitations. The current mobile target is the web PWA with the shared Rust
WebAssembly module. Generated iOS files and mobile CLI commands are retained as
experiments, not release targets.

Desktop artwork is owned by `../desktop/assets/codex-studio.icns`. Run
`pnpm --filter studio-native-client run icons` on macOS to regenerate the
native icon variants after changing that source.

Build the renderer from the repository root, then run the hidden desktop checks:

```sh
pnpm --filter codex-agents-web run build
cargo test -p studio-client-domain --lib --locked
cargo test -p studio-client-core --lib --locked
cargo test -p studio-native-client --lib --locked
cargo build -p studio-native-client --features native-check --locked
python3 workspaces/client/apps/native/check-native.py
```

The check feature uses temporary client stores and isolated HTTP fixtures. It
checks native IPC, durable preference imports, frame origins, HTTP and SSE, and
draft persistence after restart with the backend offline. It does not prove
migration of an installed Electron profile or a live model request.

The host attaches to a verified existing backend, or starts one after confirming
its absence. It restores the saved launch environment and requires a compatible
supervisor when supervisor mode is enabled.
Local data uses a separate native SQLite client store. Importing an installed
Electron profile and secure remote-server credentials, then verifying the final
package, remain release prerequisites. Fixture checks do not establish that the
installed profile has been migrated.

The renderer loads preferences before importing application modules. Its Storage
adapter commits through Rust and keeps pending request IDs in a WebView recovery
journal. Restart replays those exact requests before reading the SQLite snapshot;
closing waits for acknowledgement. The combined shell gives each server an immutable
owner and storage namespace. Native bootstrap imports explicitly mapped source
profiles into the shared renderer before application modules load. Conflicting
preferences or databases stop the import without choosing one source over another. Most hidden checks use ephemeral WebView
stores. Separate persistent checks write a Blob in profile A, verify that profile
B cannot read it, and reopen A to verify the exact bytes.

An explicit `CODEX_DESKTOP_PROFILE` uses a WebKit store derived from its canonical
path, as well as its own SQLite store. This requires macOS 14 or newer. Older
systems reject an explicit persistent profile instead of using the default store.
The default installation retains its default WebKit store. Moving an explicit
profile to another path selects another WebKit store; copy or import browser data
before changing that path. An ephemeral test profile does not retain WebKit data.

Local file open, reveal and preview requests use the Rust file resolver. The
host discards renderer permits and obtains a single-use permit from a recent
native input event on the UI thread. Remote server services cannot resolve files through
the local backend. Hidden checks verify rejection without native input; they do
not open Finder or validate a physical click.

The core's process-wide audio adapters have one connection owner. Other
connections receive `audio_busy` until that owner disconnects. Disconnect cleanup
runs after pending requests stop, including malformed IPC, and revokes microphone
grants and reservations. The Tauri renderer uses native microphone capture,
Speech transcription and WebRTC voice. Existing recordings stay in browser
storage. The shell binds audio permits, uploads, subscriptions and sessions to
their originating server services. Pending operations retain that owner when the
user opens a different chat.
Tests cover isolated PCM, WAV uploads, cancellation and peer lifecycle. They do
not establish that a real microphone or external voice service works on this Mac.

The host also exposes folder and file pickers, Finder reveal, file saving,
notifications, and sign-in links. A remote sign-in must match its explicit server owner.
The host waits for the local backend to confirm the redirect port before opening
the browser. Vault paper keys open only in the person's Terminal through the
bundled vault tool; their output is not captured by Studio. Hidden checks reject
these privileged actions without native input and do not launch Terminal.

Local writes currently cross asynchronous Tauri IPC. The renderer preserves
request identities after uncertain replies and waits for pending draft commits
before accepting a close request. `TODO(attar-sync)` comments identify paths to
restore to synchronous durable commits when the renderer moves to Attar.

Profile export version 2 stores the database schema and preferences in a manifest,
with record batches in its adjacent `.parts` directory. Keep both together.
Each batch has a SHA-256 checksum and byte and record counts. The native host
serves only batches referenced by the requesting owner's snapshot. The browser
checks all batches before taking ownership of an empty destination, then imports
one batch at a time. An interrupted import rebuilds only that import's destination;
a completed retry preserves later edits. Version 1 archives remain readable.
Exports read 32 records per transaction and normally write batches below 8 MiB.
An encoded record above 64 MiB is rejected explicitly. The exporter requires the
source desktop to be closed; its backend can remain running. A copy taken while
the desktop runs is suitable for diagnostics, not a final migration snapshot.

## Package cycle

`package.mjs` builds the Tauri client and the Rust runtime binaries, signs the
bundle with the configured identity, and writes the app to
`CODEX_NATIVE_PACKAGE_OUT`. `check-package.mjs` and `check-package-start.py`
verify it. The Rust release build is the longest step. Do it once per commit:

```sh
just build-release-binaries      # once per commit; writes studio-release-manifest.json
just package-native /tmp/out     # host check, reuse binaries, package, checks
just package-native /tmp/out --hosted-ci-passed
```

- `just build-release-binaries [dir]` builds the release binaries for `HEAD` into
  a Cargo target directory (default `CODEX_RELEASE_TARGET_DIR`, else
  `~/.cache/codex-studio/release-target`). It records the commit and the SHA-256
  of every binary in `release/studio-release-manifest.json` next to them. A dirty
  tracked tree is refused.
- With `CODEX_RELEASE_TARGET_DIR` set, `package.mjs` (native and Electron) skips
  its own Cargo build and uses that directory, which also holds the Tauri
  build. It refuses binaries from another commit, a changed binary, or a dirty
  tree. A rehearsal can use the same directory (`just verify-release-binaries`
  checks it) instead of building a second copy in a private target.
- `just package-native` runs the dev-profile host check first (`just
test-native-webview`). `--hosted-ci-passed` skips it; use the flag only
  when hosted CI passed on the exact commit. The default runs the check.
