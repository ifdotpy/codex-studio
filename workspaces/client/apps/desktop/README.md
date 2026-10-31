# Desktop packaging and recovery helpers

## Change Contract

The Electron application is removed (#630). The Tauri host in
[`../native`](../native/README.md) is the desktop app. This directory keeps the
parts that the native host, `just deliver-server`, the upgrade canary, and peer
updates still use:

- `api.ts`: the bridge contract that the web client imports.
- `backend.ts`, `recovery.ts`, `restart-environment.ts`: backend identity and recovery configuration.
- `recover_backend.py`, `install_update.py`, `deliver_server.py`, `desktop_*.py`: recovery, update, and delivery helpers.
  The server delivery path (`workspaces/runtime/apps/runtime-server/deliver.ts`) runs them. Do not delete them before a
  TypeScript or Rust port replaces them.
- `package.ts`, `signing*.ts`, `release-binaries.ts`, `layr-binary.ts`, `runtime-binaries.ts`, `python-resources.ts`,
  `bundle-name.ts`, `native/`: the resources and signing that `../native/package.ts` bundles.
- `speech.ts`, `frame-owner.ts`: shared with web tests.
- `export-profile.ts`, `export-browser-profile.ts`, `export-server-credentials.ts`: the export of an old Electron
  profile. This is the only code that still needs the `electron` dev dependency.

Do not access SQLite or provider credentials from the renderer, and do not stop a backend to update desktop source.
Preserve state-directory and bundle identity. Package commands are owned by [`package.json`](package.json). Focused check:
`pnpm --filter codex-agents-desktop run test`.

## Package

`pnpm run package` builds the signed helper resources into `CODEX_DESKTOP_PACKAGE_OUT` (default `dist`). The native
package flow calls it with the same output. Follow the [native package cycle](../native/README.md#package-cycle) and the
[signing setup](../native/README.md#signing-setup). Run `just signing-check` before you package.

To skip the Rust release build when the binaries already exist for the same commit, build them once with
`just build-release-binaries` and set `CODEX_RELEASE_TARGET_DIR`. The packager checks the commit and SHA-256 manifest.

The package writes `studio-build.json` and includes `install_update.py`. Paired servers install a newer build with it.
See [Updates from a paired server](../../../../docs/peer-updates.md).

## Recoverable dictation

The composer microphone saves recordings for the selected chat. Audio stays available for playback, download, retry, and
deletion after a reload. Transcription uses the on-device macOS recognizer in `native/speech.swift` and requires explicit
microphone and Speech permission. The checks mock capture and recognition.
