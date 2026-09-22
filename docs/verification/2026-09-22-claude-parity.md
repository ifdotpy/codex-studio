# Claude integration parity

Reference: T3 Code commit `c26119ada3565dda55bc3e86fdf47c7f9088e4b5`.
Scope: its Claude Code subscription integration, within Studio's existing UI.
The owner selected subscription authentication. API routers remain outside this scope.
Existing Codex sessions and accepted message identities must remain intact.

| Capability | Acceptance check | State |
| --- | --- | --- |
| Subscription windows and streamed updates | 5-hour, weekly, model windows; percentages and reset times | Verified below |
| Live Steer and explicit queue | Same turn identity; no lost or repeated message | Verified below |
| Session model and effort changes | Native setter between turns; capability validation | Verified below |
| Thinking, fast mode, plan and permission modes | Settings reach Claude; plan approval does not execute work | Verified below |
| Native tools, progress, subagents and background tasks | Native identities; status and output survive final reply | Verified below |
| Questions and MCP dialogs | Answers, denial and cancellation return to the exact request | Verified below |
| Context and compaction | Accurate active context; `/compact` and auto-compact settings | Verified below |
| Commands and skills | Discover native commands; invoke from composer | Verified below |
| Resume, fork and rollback | Saved native boundaries; repeat request cannot duplicate a fork | Verified below |
| Multiple Claude configurations | Config directories and account identities stay separate | Verified below |
| Provider status and configuration | Binary, launch settings, custom models, version and sign-in status | Verified below |
| Attachments and assistant text | Native image/document inputs; no omitted visible assistant output | Verified below |

Voice and Studio command monitors are separate Studio features, not Claude adapter
capabilities in the reference. Native Bash remains the command execution path.

## Automated evidence

- `tests/claude-bridge-contract.py`: 13 deterministic native protocol tests.
  Includes persistent turns, exact Steer identities, attachment/result race,
  permissions, plan denial, visible thinking, late subagent output, background
  task completion, commands, and unknown-history holds.
- `tests/claude-controls-contract.mjs`: 12 tests for native commands, fork UUIDs,
  rollback, lost receipts, failed persistence, and selected turn boundaries.
- `tests/claude-features-contract.mjs`: 6 tests for native windows, stream units,
  token accounting, tools, and the persistent input queue.
- `tests/claude-controls-runtime-contract.py`: 6 isolated SQLite tests for
  workspace reservations, rollback recovery, queues, settings, and idle retirement.
- `tests/claude-provider-contract.py`: 5 tests for routing, Steer, profile defaults,
  subscription isolation, and separate command batches.
- `tests/claude-profiles-contract.py`: 5 tests with isolated native authentication
  fixtures. Additional account tests: 10 account-store and 11 runtime tests.
- `tests/claude-parity-update-contract.py`: 4 tests against `c4af1ae`, with live
  function identity, HTTP closure, exact source guards, and rollback on failure.
- `tests/runtime-contract.py`: 50 existing orchestration checks passed.
- Production TypeScript and Vite build passed. Existing Limits browser tests passed.
  Extended account browser tests cover Claude windows, profiles, Steer, settings,
  commands, and rollback. Lost HTTP response cases preserve command identities
  across a renderer reload and reuse the same rollback receipt. Completed rollback
  clears the removed selection. The hidden Electron desktop test passed.

## Real native evidence

CLI `2.1.278`, SDK `0.3.278`, Claude subscription, isolated temporary projects:

- Native usage read returned five-hour and weekly windows and a model window.
  The probe sends no model prompt and skips transcript behavior scans.
- Two Haiku turns used one persistent query. Rollback removed the second turn.
  A repeated rollback request returned the same receipt. The next answer recalled
  only the first turn, `PARITY_ONE`.
- Fork at the first turn retained exactly that turn. A branch in a separate
  directory resumed successfully and supported a subsequent rollback.
- During a native Bash `sleep 2`, Steer changed `FIRST` to `SECOND` in the same
  logical turn. Native command discovery returned 70 commands.
- `/compact` executed through the native command path without Studio text wrappers.
- A real Studio Runtime session called `orchestration_title` and completed with
  `STUDIO_TOOL_OK`.

Permission denial, background completion, multiple-profile isolation, and optional
model flags use deterministic protocol evidence. They are not live model tests.
Non-image attachments use Studio's existing file-reference path and Claude's native
Read tool. Image inputs use the native base64 image block.

## Scope and limits

The comparison uses the Claude subscription path at the reference revision above.
API key routers and non-Claude provider features are outside this change.
Studio command monitors, voice, and cross-account transfers remain unsupported.
The experimental SDK usage method may change with a future SDK release; this
release pins its dependency and rejects missing native limit data visibly.
Unknown native fork outcomes retain a recovery hold. They never trigger an
automatic repeated fork or discard the source history.

The new live patch supersedes the initial Claude installation patch. Its baseline
and source hashes are separate. Historical initial-install patch fixtures do not
validate this release. The existing general live-update fixture also has a known
historical message-intent signature mismatch unrelated to this change.

## Deployment

Installed in `/Applications/Codex Studio.app` on 2026-09-22.
Live patch `2026-09-22-claude-parity-v1` applied on its first attempt.
Backend PID remained `55850`; no active user agent was stopped.
The installed `/api/limits?account_key=claude-local` read completed in 4.16 seconds
with no error. It returned five-hour, weekly, and Fable windows.
The installed `/api/claude/session` state route also returned successfully.
The packaged and installed app signatures passed strict verification.
The packaged runtime, bridge, patch, and frontend matched the reviewed source.

The general live-update suite returned 12 of 13 passing checks. Its unchanged
historical fixture rejected `Unreviewed message intent replacement`, as it did
before this task. The four release-specific guarded-update checks passed.
