# Claude Code subscription support

Studio uses Claude Agent SDK 0.3.278 and the installed Claude Code executable.
The live checks used Claude Code 2.1.278 and a signed-in Max subscription.
Credentials remain in Claude Code. No API key was supplied.

## Observed behavior

- The installed backend returns five native model choices for `claude-local`.
- Two real Haiku turns returned `STUDIO_CLAUDE_OK`. The second turn recalled the first reply.
- A real turn through Runtime called `orchestration_title` and returned `STUDIO_TOOL_OK`.
- The Runtime check also passed with imports from the installed application.
- These model checks used separate temporary state directories.
- `/Applications/Codex Studio.app` passes `codesign --verify --deep --strict`.
- The packaged application passes the same signature check.
- Live update `2026-09-22-claude-code-v3` applied to backend PID 55850 without restarting it.
- The installed renderer index matches the production build.

## Automated checks

Passed:

- `tests/claude-bridge-contract.py`: three tests. Covers streamed text, tool results,
  permission denial, user answers, interruption, stale interruption rejection,
  duplicate message identities, native failure, and account changes before a prompt.
- `tests/claude-provider-contract.py`: four tests. Covers account selection,
  subscription identity, queue delivery, worker defaults, and API environment removal.
- `tests/claude-update-contract.py`: prior source, repeated application,
  method object preservation, and rejection of an unknown implementation.
- `tests/runtime-contract.py`: 50 tests.
- `tests/accounts-contract.py`: ten tests.
- `tests/runtime-accounts-contract.py`: 11 tests.
- `tests/accounts-ui-smoke.mjs`: account selection and limit display checks.
- `desktop/test.mjs`: hidden-window desktop checks, including backend survival.
- Production web build and desktop package build.

`tests/live-updates-contract.py` passes 12 of 13 tests. Its historical message
intent fixture fails with `Unreviewed message intent replacement`. The transcript
function has the same signature at baseline commit
`04ce45adf2cbfa97f8d7bc120259af86acb4d047` and after this change. Neither matches that
older fixture's expected signature. Claude's update has its own passing guard test.

## Limits

See the Claude section in [README](../../README.md). The provider does not implement
Steer, Studio command monitors, voice, subscription limit reads, or account transfer.
Native background tasks are disabled. Claude uses its native permission rules.
