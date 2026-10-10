# Limits cache

Source base: `0b4aa0caf88263007edc8839200d428fa751c326` (`origin/main`).
Branch: `limits-cache`.
Evidence directory: `/tmp/limits-cache-evidence-3db481b8`.

## Changes

- `accountUsage.ts` owns the existing `codex-limits:<stateDir>` cache.
- The server storage namespace separates local and remote servers.
- App restores the cache before its limits consumers render.
- Account controls subscribe to the same cache with `useSyncExternalStore`.
- Each account snapshot retains its account key, identity, and timestamp.
- Fresh values replace saved values without a limits spinner.
- A refresh error retains known values.
- Successful empty reads retain the existing baseline semantics.
- `limitsBaselineReader`, `limitRecovery`, and provider data validation retain their rules.

## Surfaces

| Surface                          | Source                             |
| -------------------------------- | ---------------------------------- |
| Composer footer and account dots | `App.tsx`, `Usage.tsx`             |
| Usage and Limits panel           | `Usage.tsx`                        |
| Chat settings account tiles      | `AccountTiles.tsx`                 |
| Project settings account tiles   | `AccountTiles.tsx`                 |
| Accounts manager tiles and cards | `AccountTiles.tsx`, `Accounts.tsx` |
| Account menu weekly values       | `Accounts.tsx`                     |
| Limit recovery notice            | The shared snapshot from `App.tsx` |

## Backend

Runtime restores limits from the existing workspace sync entity before it publishes startup state.
Every limits write saves the snapshot, including a change to its timestamp alone.
A missing workspace entity is created through the existing entity projection.

`/api/limits` returns known values immediately and starts one background read per account.
The existing provider freshness rules and provider lock still apply.
A first read without known values retains the provider read path.
The `cached=1` read retains its cache-only behavior when a snapshot exists.

## Checks

| Check                                      | Result                        |
| ------------------------------------------ | ----------------------------- |
| Web build                                  | Pass                          |
| Web unit tests                             | 573 passed, 1 skipped         |
| Backend account, cache, and resource tests | 51 passed                     |
| Strict runtime mypy                        | Pass                          |
| Selected Oxlint and Oxfmt                  | Pass                          |
| Pre-commit staged-content fixtures         | Pass                          |
| Focused browser suite                      | 3 passed, 4 existing failures |
| Expanded cache first-paint check           | Pass                          |

The unit tests cover cache restore, fresh replacement, older data, account isolation, unknown values, empty reads, and refresh errors.
Backend tests cover snapshot restore, real Runtime restart, timestamp persistence, a slow provider, and one background read per account.
The resource tests retain the rule that timestamp changes alone do not publish a limits notification.

## First paint

The browser check holds limits responses before it opens the panel.
It then waits three seconds after the panel opens before it releases the responses.
Saved values appear while every limits response remains blocked.
The fresh response changes the panel from 58% to 90% remaining.
After reload, the panel immediately shows 90%.
Chat tiles, account cards, and Project settings show Personal at 90% and Work at 20% while reads remain blocked.
The account without a saved value has no limit meter.
The panel has no limits loading text or button spinner over known values.

The same check on `origin/main` fails because the Personal account tile has no meter during the blocked read.
Screenshots show the missing bars before the change and the saved bars after the change.

## Existing browser failures

| Test                                 | Current failure              | Baseline evidence                         |
| ------------------------------------ | ---------------------------- | ----------------------------------------- |
| `account-limit-dots-ui`              | Chat click, line 296         | Same failure on `origin/main`             |
| `accounts-ui-smoke`                  | Account picker, line 881     | Same failure on `origin/main`             |
| `accounts reauthentication recovery` | Sign-in button, line 517     | Same failure on `origin/main`             |
| `limits-ui`                          | Initial chat click, line 210 | Same failure in the separate baseline run |

A second baseline `limits-ui` run proceeds farther and fails at line 431.
The baseline logs record both failures.
These legacy checks remain unresolved.

## Evidence files

All paths below use the evidence directory above.

- `before-account-tiles.png`
- `after-account-tiles.png`
- `cached-panel-before-response.png`
- `fresh-panel.png`
- `cached-panel-after-restart.png`
- `cached-account-cards.png`
- `cached-project-account-tiles.png`
- `logs/build.log`
- `logs/unit.log`
- `logs/backend.log`
- `logs/focused-browser.log`
- `logs/baseline-browser.log`
- `logs/baseline-limits-initial-failure.log`
- `logs/first-paint.log`
- `baseline-results/` (failure traces)
- `final-firstpaint-results/` (screenshots)

No installed app, live backend, or port 4620 was used.
