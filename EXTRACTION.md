# Application extraction

The application was extracted from `engineering-skills/codex-agents` on 2026-09-06.
The source commit is `fe89689`. A Git subtree split produced
`20f4a75fca546f20c936d7753394f57e7b07c010`, with 34 commits of component history.
The standalone repository starts from that history. It has no remote configured.
The original license is retained without new permissions.

The application owns `scripts/`, `prompts/`, `tests/`, `web/`, `desktop/`, and its
product documentation. The full command guide moved to `CLI.md`. The application
does not need the skill checkout to import modules, build, run, or start workers.
Runtime worker prompts remain with their launcher.

The separate skill retains `SKILL.md`, `agents/openai.yaml`, and one small
`codex-board` compatibility entrypoint. That entrypoint forwards old external
callers to the installed command. It contains no board implementation.
The installed skill link remains `~/.agents/skills/codex-agents`.

The command installer creates 13 links in `~/.local/bin`, including the existing
`luna` command. It resolves implementation paths into this repository. It checks
all conflicts before changing links and refuses unrelated files or link targets.
The skill finds application documentation through the resolved command location.

Database, model profiles, wave state, resource claims, and browser storage retain
their existing locations. The installed Electron package is self-contained and
can attach to the relocated backend without a package rebuild.

## Checks

- Three command installer contracts: repeated install, external-cwd execution,
  foreign-file preservation, and exact previous-link migration.
- Portable command and state contracts, one daemon contract, 42 runtime
  contracts, and 17 canvas contracts pass from the standalone checkout.
- The React production build and product browser suite pass from this checkout.
- Twenty hidden Electron checks pass from this checkout, including backend
  attachment, native callers, and persistence after application exit.
- The skill validator passes. Installed-command discovery resolves this project.
- The old skill board path executes the installed command with isolated state.

These checks use fixtures. They make no paid model requests and consume no real
reset credits. Historical delivery documents retain their original evidence.

## Local transition

The live server now starts from `/Users/igor/Projects/codex-agents/scripts/codex-canvas` (PID `37912` at verification).
Its URL and state directory remain unchanged. Both existing agents remain.
The database backup is `/Users/igor/.local/state/codex-agents/backups/canvas-before-source-extraction-20260906-104252.sqlite3`.

After removal of the old source paths, the installed Electron application attaches
to this backend and displays the client built from this repository. Verification
uses a hidden window with a separate Electron profile because the user's normal
application is already open. Closing the test instance preserves the backend.
The installed `codex-control list` command also reads the same runtime.

Evidence: `~/.local/state/codex-agents/evidence/source-extraction-20260906/`.
Old ignored web build files were preserved outside the skill at
`~/.local/state/codex-agents/migration-backups/skill-build-cache-20260906/web/`.
Unrelated old Python cache files remain untouched.
