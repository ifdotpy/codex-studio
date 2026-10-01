# Progress preview and expansion

The panel shows supported progress content even when it exceeds the height budget.
The collapsed preview has a fade. Expand shows a scroll area up to half the viewport.
Collapse restores the top preview. The PROGRESS.md link still opens the complete file.

The collapsed budget uses 28% of viewport height, from 100px to 280px.
Phone controls have a minimum size of 44px.
Unsupported Markdown still shows a notice and the file link.
Cached copies keep their label and support expansion.

Renderer v2 reports overflow pixels, complete top-level lines or list items,
and the last visible line and heading. A paragraph or heading counts as one line.
The tool puts a short hint before the reports. Clipped content returns exit 0.
Unsupported content and file errors return exit 1. No current measurement returns exit 2.
Exact revisions, content hashes, client identities, sequences, and 90-second expiry remain checked.
Legacy clients that hide overflow still report failure until they update.

Agent rules now put current status first and allow overflow.
Agents trim only when important lines are hidden. They do not repeat rewrites to remove overflow.

## Checks

- `tests/progress-layout-contract.py`: visibility data, exit codes, multiple clients, revisions, replay, expiry, and HTTP access checks.
- `tests/progress-fit-browser.mjs`: fit, clipping, expansion, headings/items, narrow widths, fonts, unsupported content, and report lifecycle.
- `tests/progress-markdown-panel-browser.mjs`: reads, cache labels, errors, clipping, expansion, and unchanged composer/history.
- `tests/progress-cache-browser.mjs`: durable cache and scope boundaries.
- `tests/progress-cache-ui.mjs`: production renderer, cached overflow after reload, empty revisions, and phone layout.
- `tests/progress-file-contract.py` and `tests/progress-runtime-contract.py`: file and prompt boundaries.
- `tests/token-efficiency-contract.py`: prompt delivery and existing agent message behavior.
- `npm --prefix web run build`: TypeScript and production bundle.

Browser checks use hidden Chromium and WebKit. The production fixture uses an isolated temporary state directory.
No live progress file changes or model calls occur.
The runtime progress test now records the existing context manifest when its fixture marks an event delivered.
The HTTP fixture now supplies the existing sync sequence method.

## Screenshots

Baseline: `e98b69a`. Both panel previews use the same list of 50 items.

- [Before, 1440px](progress-fit/before-1440.png)
- [After, 1440px](progress-fit/after-1440.png)
- [Before, 390px](progress-fit/before-390.png)
- [After, 390px](progress-fit/after-390.png)
- [Application, 1440px](progress-fit/app-after-1440.png)
- [Application, 390px](progress-fit/app-after-390.png)
- [Expanded application, 1440px](progress-fit/app-expanded-1440.png)
- [Expanded application, 390px](progress-fit/app-expanded-390.png)

Regenerate after screenshots with `PROGRESS_SCREENSHOTS=after node tests/progress-fit-browser.mjs`
and `PROGRESS_SCREENSHOTS=after node tests/progress-cache-ui.mjs` after a web build.
