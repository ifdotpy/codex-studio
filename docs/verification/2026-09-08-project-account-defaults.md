# Project account defaults

The project stores one default account. New chats select this account automatically.
An explicit account choice applies to that chat. Existing chats retain their account.
Workers retain their parent account and native thread identity.

Studio no longer treats account project folders as access limits. Files and skills
can be outside the project directory. Native sandbox and approval checks remain.
The old account rules endpoint, editor, checks, and model instructions are removed.

Existing projects receive defaults once. Migration preserves project identities
and saved defaults. Otherwise, it uses the newest lead at the exact path, an
unambiguous legacy account rule, or the global default, in that order.
Project account writes check the saved revision. Exact retries retain the result.
New chat requests preserve their identifiers after a lost response.

## Evidence

Checks used an isolated source snapshot with temporary runtime state.

- `python3 tests/project-accounts-contract.py`: 5 tests passed.
- `python3 tests/accounts-contract.py`: 10 tests passed.
- `python3 tests/account-project-runtime.py`: 9 tests passed.
- `python3 tests/projects-contract.py`: 9 tests passed.
- `python3 tests/runtime-accounts-contract.py`: 11 tests passed.
- `python3 tests/runtime-contract.py`: 50 tests passed.
- `python3 tests/yolo-contract.py`: passed.
- Voice, steer, workspace recovery, and rule/monitor checks passed.
- `node tests/account-project-ui.mjs`: mock API browser checks passed.
- `node tests/accounts-ui-smoke.mjs`: passed.
- `node tests/mobile-client-ui.mjs`: real Runtime browser checks passed.
- `npm --prefix web run build`: passed.

The real Runtime browser check covers project defaults, an explicit chat account,
repeated New chat actions, another project, and exact requested chat identities.
The mobile dialog screenshot was inspected at 320 pixels.
An independent source review found stale help text. That text is corrected.

The obsolete account denial suite was removed. Replacement checks prove access
with legacy rules present. Native permissions and duplicate request checks remain.

## Limits

These checks do not use a live model or prove a model's response.
The installed application and active backend were not updated or restarted.
Active agents and stored user state were not changed.
