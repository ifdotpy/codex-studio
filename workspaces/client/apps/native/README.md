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

The host attaches to a verified existing Rust backend, or starts the packaged
`studio-runtime-server` after confirming its absence. It restores the saved launch environment and requires a compatible
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

Set up package signing first. See [Signing setup](#signing-setup) and run
`just signing-check` before you package.

`package.mjs` builds the Tauri client and the Rust runtime binaries, signs the
bundle with the configured identity, and writes the app to
`CODEX_NATIVE_PACKAGE_OUT`. `check-package.mjs` and `check-package-start.mjs`
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
- `just package-native` first runs the signing check (the same check as
  `just signing-check`). A signing problem fails before the long build.
  Package validation and the hidden startup check run with Node and require
  no Python backend environment or PyO3 module.
- `just package-native` runs the dev-profile host check next (`just
  test-native-webview`). `--hosted-ci-passed` skips it; use the flag only
  when hosted CI passed on the exact commit. The default runs the check.

## Signing setup

Every package is signed with one persistent certificate. A new certificate
changes the application identity, and macOS then asks again for the permissions
that the user already gave. The build refuses ad hoc signing. The signer is
[signing.ts](../desktop/signing.ts).

Agents must run `just signing-check` before `just package-native`.

### Canonical paths

| Item              | Path                                                             |
| ----------------- | ---------------------------------------------------------------- |
| Configuration     | `~/.config/codex-studio/signing.json`                            |
| Signing folder    | `~/.local/state/codex-studio-signing/current` (a symlink)        |
| Keychain          | `~/.local/state/codex-studio-signing/current/Studio.keychain-db` |
| Keychain password | `~/.local/state/codex-studio-signing/current/password`           |

The `current` symlink points to the folder that holds a keychain and the
password file that belongs to it. To change to another pair, change the symlink.
Do not change the paths in `signing.json`.

### signing.json

```json
{
  "identity": "<40 hexadecimal characters: the certificate fingerprint>",
  "keychain": "/Users/<you>/.local/state/codex-studio-signing/current/Studio.keychain-db",
  "passwordFile": "/Users/<you>/.local/state/codex-studio-signing/current/password"
}
```

- Use absolute paths. The file does not expand `~`.
- `identity` is the SHA-1 fingerprint. Find it with
  `security find-identity -p codesigning <keychain path>`.
- `keychain` and `passwordFile` are optional. Without them, the build looks in
  the default keychain search list and does not unlock anything.
- `CODEX_STUDIO_SIGNING_IDENTITY` (and `CODEX_STUDIO_SIGNING_KEYCHAIN`) override
  the file. An override cannot name a password file. Unlock that keychain
  before the build.
- The password file holds only the keychain password. The build sends it to
  `security` through standard input. It never appears in arguments, logs or
  errors.

### File modes

Set the mode to `0600` for `signing.json` and the password file, and `0700` for
the signing folder:

```sh
chmod 600 ~/.config/codex-studio/signing.json ~/.local/state/codex-studio-signing/current/password
chmod 700 ~/.local/state/codex-studio-signing/current/
```

Never commit, copy or print these files, the keychain or any exported key.

### Check the setup

```sh
just signing-check
```

The check reads the same configuration, unlocks the keychain the same way and
looks for the identity. It takes a few seconds. It never prints the password or
key material. On success it prints one line:

```text
PASS: keychain <keychain path> identity <fingerprint>
```

On failure it prints `FAIL:` with the exact reason and exits with status 1.

### Common failures

| Message                                                                        | Meaning and fix                                                                                                                                              |
| ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Signing configuration ... does not exist`                                     | No `signing.json`. Create it as shown above.                                                                                                                 |
| `... has mode 0644. Run: chmod 600 ...`                                        | Group or other can read the file. Run the `chmod` command in the message.                                                                                    |
| `... is not valid JSON`                                                        | Fix the JSON syntax in `signing.json`.                                                                                                                       |
| `"identity" ... must be a 40 character hexadecimal ...`                        | Copy the fingerprint from `security find-identity -p codesigning <keychain>`.                                                                                |
| `Keychain file ... does not exist`                                             | The path or the `current` symlink is wrong. Check `ls -l ~/.local/state/codex-studio-signing`.                                                               |
| `Password file ... does not exist`                                             | The path is wrong or the file is missing from the signing folder.                                                                                            |
| `Could not unlock ...: The user name or passphrase you entered is not correct` | The password file does not belong to this keychain (security status 51). Use the password file from the same folder as the keychain. Do not guess passwords. |
| `Could not unlock ...: The specified keychain could not be found`              | The keychain path is wrong, or the file moved.                                                                                                               |
| `Could not unlock ...: ETIMEDOUT`                                              | `security` did not answer in 10 seconds. Run the check again. Look for a keychain dialog on the screen.                                                      |
| `The configured Studio signing certificate ... is not in keychain ...`         | The fingerprint is not in that keychain. Compare it with `security find-identity -p codesigning <keychain>`.                                                 |
