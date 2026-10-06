# Text clipping detector

Import `detectTextClipping` from `text-clipping.mjs`.
Call it with `page.evaluate(detectTextClipping, { scene, profile })` after the page settles.

The report contains the scene, profile, selector, text, rectangle, and reason.
It checks text ranges against hidden or clipped ancestors, dimensions, ellipsis
without full text, and covered button or label text. A title, an accessible
label, or a referenced tooltip must contain the full text. A line clamp with
full text has `intentional: true`.

The detector excludes hidden elements, closed details, text outside scroll
viewports, screen-reader text, and controls below a modal. A scroll container
permits text outside its viewport. Vertical scroll overflow has an intentional
report entry. Open menus and model lists can intentionally cover controls. A sticky modal
header can cover controls in the same scroll container. These cases have a
justification in the report. Native select options, input values, SVG icons,
and terminal canvas cells require separate checks.

Run the detector regression test with one worker:

```sh
cd web
npx playwright test --config playwright.config.ts --project=client --workers=1 text-clipping.spec.mjs
```

The scene runner is `/tmp/ui-compare/clip-audit/run.mjs`. See its README for the
isolated fixture, five profiles, screenshots, and JSON report.
