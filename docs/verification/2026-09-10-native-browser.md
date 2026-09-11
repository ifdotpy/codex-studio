# Native Chrome skill integration

Studio registers the desktop's installed Chrome skill directory through native
`skills/extraRoots/set`. Codex advertises the skill and its absolute path to the
model. Studio passes the existing `node_repl` runtime configuration to native
`thread/start` and `thread/resume`. The same path serves leads and workers.
Studio adds a short default preference, not the skill text or browser code.

## Boundaries

- Runtime paths come from the main Codex profile's installed desktop runtime.
- The `chrome/latest` symlink selects the installed skill version.
- Each browser service receives the selected account's `CODEX_HOME`.
- Credentials and unrelated MCP servers are not copied.
- Account-local runtime settings, custom commands, and explicit disables remain.
- Disabling integration removes Studio's host skill root on next preparation.
- Missing runtime or skill files produce no availability claim.
- Host root registration is cached per connection and version. Repetition after
  an unknown response is safe because the operation sets the complete root list.
- Studio owns these app-server connections and centralizes extra root registration.
- Loaded threads can ignore resume overrides while subscribed. Idle-session
  recovery requires native unsubscribe followed by resume with current settings.
  Active work is not interrupted. Native skill catalog updates can arrive separately.
- Website permissions remain with the native integration.
- This change does not implement or certify an extension window border.

## Source evidence

Inspected Codex `rust-v0.153.4`, commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`:

- `codex-rs/app-server/tests/suite/v2/host_skills.rs`: host skill advertisement
  and catalog changes between turns.
- `codex-rs/ext/skills/src/host_service.rs`: extra root registration.
- `codex-rs/core-plugins/src/marketplace_policy.rs`: bundled marketplace locations
  are account-specific. A cross-account plugin install was rejected. Studio uses
  the supported host skill API instead of importing another account's marketplace.

The desktop application's existing configuration supplies browser runtime paths.
No bundled application source is copied into this repository.

## Verification

Python 3.14: 82 tests passed across `browser-native-contract.py` (13),
`role-skills-contract.py` (8), `runtime-contract.py` (50), and
`runtime-accounts-contract.py` (11).

A separate native app-server with Lumina's home accepted the host skill API.
`skills/list` returned enabled `chrome:control-chrome` with its installed absolute
path. Ephemeral thread 01a08b07-977a-7f01-920c-957da29029d2 exposed native `node_repl` tools.
This probe did not start a model turn.

The live Studio server was updated without restarting its four account
connections. Actual Studio lead 00c52e3d-3492-4f9c-add8-c93f092df431, native thread
01a08b09-2cf7-7a70-861c-f6a0ad01de39, used Lumina's account. Its prompt requested only a page title
and screenshot, without tool names or skill paths.

Its first command read the correct `SKILL.md` directly, with no filesystem
search. Native `node_repl.js` selected Chrome with browser type `extension`,
opened `https://example.com/`, returned `Example Domain`, and emitted a screenshot.
The turn completed and the test tab remains open. Worker integration is covered
by the common preparation path and role contract tests; no new live worker test
was run for this update.

The earlier main-account Studio test also verified native cursor movement.
Existing loaded sessions are not retroactively converted from another browser tool.

## Existing Twoj Startup session recovery

The existing lead `86333c95-f7a0-4980-9fe2-79a428c4fe8b` still received
`Browser is not available: chrome` after the new-session integration passed.
A fresh native call in that same session confirmed the failure. The lead then
finished its turn and preserved the existing page.

At an idle boundary, Studio called `thread/unsubscribe`, then `thread/resume`
with the current browser configuration and developer instructions. Native thread
`01a08a79-ea43-7f10-af88-220f16226188` and account connection
`be9e1bb3-599a-4cd8-9802-4f1bc5add365` remained the same. No account server,
worker, or monitor was restarted. The temporary dispatch hold was released.

The next real model turn selected Chrome with type `extension`, found the
existing e-Doreczenia registration tab `1455651035`, read its title, and emitted
a screenshot. Visual inspection confirmed the authenticated registration page.
The check made no form changes or external submissions. The lead received a
message to continue its authorized work through the restored native connection.

The recovery is verified. The earlier browser discovery failure's internal cause
is not established. Do not infer that Computer Use or the separate browser-use
package was repaired. The native source in `thread_processor.rs`,
`resume_running_thread`, confirms that a subscribed loaded thread can ignore
changed resume settings. Unsubscribe at an idle boundary permits a fresh load.
