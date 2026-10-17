# Native desktop parity, 16 October 2026

The native host intercepted drag events before the existing HTML sidebar,
queue, and attachment handlers could receive them. Its WebView now disables
that interception. Server labels no longer have a three-character width limit;
long names wrap and retain their full tooltip.

## Restored desktop behavior

- Project, chat, folder, team, and pinned-chat pointer reordering uses the
  existing owner-aware renderer handlers. Keyboard reordering remains available.
- The message queue accepts pointer reordering and keeps the durable order after
  reload. Dropped files follow the same upload and queued-asset path as picked files.
- View provides Reload, Actual Size, Zoom In, and Zoom Out with keyboard shortcuts.
  Reload requires the same draft, preference, and pending-write acknowledgements
  as window close. Failed saves leave the editor usable and report the failure.
- Normal window position, size, and maximized state survive restart. Saved
  geometry is constrained to available displays. Writes are debounced; hidden
  automated windows neither restore nor overwrite user geometry.
- The installed application's menu exposes background recovery using the existing
  Electron recovery implementation and its active-process protections. Starting
  the client records its exact PID/start-time open intent; graceful exit withdraws
  it. Starting a client does not reconfigure LaunchAgents. A recovered default
  profile keeps the default WebKit store.
- New-window requests remain denied, matching the Electron host.

## Audit of retained behavior

The renderer still owns folders, pins, teams, search, account selection, project
and chat actions, and queue editing. The native bridge retains file dialogs,
file actions/reveal/save, vault keys, external links, sign-in forwarding,
notifications/navigation, microphone/speech cancellation, credentials, and
owner-scoped server transport. Native Edit and Full Screen menu commands remain.
WebKit already reloads a terminated content process. Its recovery mechanism is
not the Electron-specific bounded Chromium retry/fallback implementation.

The supervisor framing repair from `7d887afd1` is included as `1b4a3c105`:
the shared frame limit remains 32 MiB. Packaging must preserve that repair and
the existing signing identity.

## Verification

- 12 sidebar browser scenarios passed: pointer and keyboard order, folders,
  teams, pins, source-server actions, offline state, and full server labels at
  desktop and narrow widths.
- The queue browser scenario passed with a dropped file, pointer reorder,
  durable reload, edits, and an uncertain-request retry retaining its identity.
- Web TypeScript, production build, and 986 unit tests passed; one test skipped.
- Recovery contracts: 23 passed, one skipped. Installed native-path acceptance
  and rejection of temporary application paths are covered.
- Native library tests: 8 passed. The complete hidden WebView command passed,
  including menu presence and existing privilege/transport/profile checks.
- Review covered close receipt identity, profile selection, recovery helpers,
  packaged resources, and preservation of the supervisor limit.

These browser tests exercise renderer pointer events and file-drop handling.
They do not inject an operating-system Finder drag. Geometry restoration and
actual LaunchAgent reconfiguration are not exercised against the user's live
profile. No load tests, live model requests, or backend restart were performed.
The full repository fast tier was not rerun for this client-only update.

## Packaged startup isolation

The initial package-start check timed out before profile creation. A process
sample located the main thread inside AppKit's persistent-window restore alert,
not in Studio or certificate validation. The package check now passes Apple's
`ApplePersistenceIgnoreState` argument for that hidden test process. This keeps
AppKit restoration separate from the user's installed app, just as the test
already isolates the Studio profile and backend state. It does not change user
defaults or delete any saved window, chat, or draft data.
