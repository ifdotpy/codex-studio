# Duplicate Studio panels

## Cause

The installed HTML loaded `index-BJjwpe1B-remove-sending-a1536aa842830557.js`.
Lazy chunks, including TerminalDock and ClaudeSettings, imported the older
`index-BJjwpe1B-lumina-7b64b62a1408c6c2.js` entry.
Both entries mounted React on the same root element. Each URL had its own module
identity. The resulting UI contained duplicate Team or Terminals panels.

## Source fix

`web/src/main.tsx` marks the root element before mounting Studio. Another entry
URL cannot mount a second tree. The browser regression imports the entry under
a second URL. It checks sidebar identity, panel counts, Team close and reopen,
and navigation to a worker.

## Checks

- Source build: API contract check, TypeScript, and Vite passed.
- Source browser regression: passed, 10.9 seconds.
- Fault injection with the mount guard disabled: failed on sidebar identity.
- Original installed asset graph: failed, two Terminals panels instead of one.
- Repaired installed asset graph: passed, 4.7 seconds.
- Native window after reload: one Team panel and one Terminals panel.

The isolated browser fixtures use temporary state and fake agent sessions.
The native check proves the panel count. It does not prove a live model request.

## Installed repair

Build `d8aef1cdd79f6033` replaces the full asset graph under fresh filenames.
All lazy imports point to the same entry as the HTML. Old asset URLs remain
available for existing windows. The application signature passed verification.
Backend PID 75638 stayed active. No agent or monitor was stopped.

The previous HTML and service worker are saved in:

```text
~/.local/state/codex-agents/app-backups/single-entry-20261006-d8aef1cdd79f6033
```

The next complete application package must include the source fix. Rebuild the
whole UI when changing an entry. Keep lazy imports and the HTML entry consistent.
