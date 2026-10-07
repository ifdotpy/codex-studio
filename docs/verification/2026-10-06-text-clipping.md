# Text clipping audit

Base: `40e2e347deea2c2953f581945d54198a6b635f4b`.

The final fixture audit passes all 80 scene and profile pairs.
It also opens request answer forms, model lists, and project menus.
Profiles: 1440x900 dark and light, 1280x800 light, 390x844 dark,
and 1440x900 dark with Appearance text at 24px.
The mobile terminal is unavailable in the shared harness.
The mobile sidebar scene runs only in the mobile profile.

The detector is in `tests/client/layout/text-clipping.mjs`.
The runner and isolated fixture are in `/tmp/ui-compare/clip-audit/`.
See their README files for usage.
All heavy commands use `/tmp/ui-compare/heavy.sh`.
Playwright uses one worker.

## Results

- Before: 428 candidates, 377 without an intentional classification.
- After: 166 candidates, zero without an intentional classification.
- The before count includes repeated surfaces and detector false positives.
- The final report replaces Model and Tools rows with focused follow-up runs.
- Raw reports remain available for comparison.

Reports: `before-reviewed/report.json` and `final-reviewed/report.json` under
`/tmp/ui-compare/clip-audit/`.
The full final run is in `final/`.
Focused runs are in `final-model/` and `final-tools-ready/`.
The Tools follow-up waits for loaded descriptions before the audit.
The base Tools follow-up is in `before-tools-ready/`.

## Corrections

- Button labels: remove Mantine text trim and permit full line height.
- Project selector: permit height growth.
- Sidebar navigation: permit height growth and expose the shared-chat title.
- Conversation header: permit larger text and retain titles for truncated text.
- Permission paths: expose full text through a title.
- Tool descriptions: expose full text through a title.
- Compact progress summaries: expose full text through a title.

Intentional report entries include vertical scroll overflow, line clamps with
full titles, open menus over controls, and sticky headers over controls in the
same scroll container. The detector excludes hidden text and text outside
scroll viewports. Native select options and terminal canvas cells need separate
checks.

Eight before/after crops were inspected. The loaded Tools comparison was also
inspected. Screenshots are in `final/comparison-*.png`.
Title changes preserve the visible truncation and provide the full text.

## Checks

- Web build: passed. The interrupted final build completed its API check.
  Recovery completed TypeScript and Vite without repeating the API check.
- Vitest: 337 passed, one skipped, zero failed.
- Detector regression: passed, including sticky headers and scroll boundaries.
- Pre-commit hook regression: passed.
- Scoped lint, format, and whitespace checks: passed.

The fixture uses isolated runtime directories and no live model requests.
Changes uses a synthetic read-only diff response.
Fixture schema startup permits 300 seconds and completes before page startup.
