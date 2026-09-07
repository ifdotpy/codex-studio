# Codex Studio panel

These rules apply only to the persistent panel in Codex Studio, not native Codex CLI.

## Compose a useful view

Use `orchestration_panel` for visual state, results, or controls between chat and composer. Each agent owns one panel. A worker cannot replace the lead's panel.

Use json-render `spec` mode by default. Call `action=catalog` for current components, properties, and expression syntax. Compose its layout primitives, stages, measured counters, charts, and controls. Do not invent component properties. Studio owns typography, colors, spacing, and control styles. Arbitrary CSS is not a structured property.

Shape and position should communicate information. Use short labels and explicit states. Distinguish a finished command from an accepted result. Never invent counts or percentages. Keep detailed evidence in chat or linked files. Update when information changes; do not poll to refresh the panel.

## Publish and inspect

Call `action=set` with `spec: {root, elements, state?}` and optional top-level `callbacks`. Elements use catalog types, properties, and child identifiers. Do not combine `spec` with `html` or `css`. Adapt [the progress example](../assets/panel-progress.json), replacing sample values with verified data.

The viewport is 150 CSS pixels high. The server validates schema and fit at widths 320, 640, and 1000px. Padding and borders belong inside that height. All selectable views must fit. Use Tabs when several detailed views need the same space. Do not conceal overflow with scrolling, clipping, or ellipsis.

The tool returns a PNG at 1000×150px. Inspect readability, structure, and fit. Catalog components ensure consistent styles, not a good composition. Validation or render failure preserves the previous content, version, and callbacks. Fix the error and submit a new call. `action=get` reads and renders the current panel; `action=clear` removes it.

## Local state and agent actions

Bind controls to local state with the catalog's expression syntax. Tabs, choices, and field edits work without a model call. Use conditional visibility for alternate views. Keep every interactive state compact, including later text input.

Use `Button` or `Form` with `props.callback` when the agent must act. Declare that callback outside `spec`, with `id`, `label`, and exact input names in `fields`. Button callbacks have no fields. Form fields submit arrays of strings.

The owner receives a `panel_callback` event with `callback`, `label`, `panelVersion`, and `values` after its current turn, including after a final answer. Stopped or deleted agents do not resume. Each callback accepts one submission per panel version. Handle it and publish an updated panel to acknowledge the result or enable another submission. An interaction does not authorize unrelated actions.

## Background data without model calls

For periodic status, counters, or EC2 resources, use a script connected through
`orchestration_panel_feed`. The agent publishes a structured panel and starts the
producer once. Each later stdout JSON object updates its bound state directly,
without a model call, conversation-history entry, or agent wake.

Keep user selections outside the feed state path. Use Studio icons and local
views instead of a scrolling list. The data must still fit the 150px viewport.
Read [the panel feed guide](panel-feed.md) for the command API, process lifecycle,
and an EC2 example with explicit instance IDs and separate resource reservations.

## HTML fallback

Use `html` and optional `css` when the catalog cannot express a needed visual, such as a custom scientific diagram. Keep the same fit checks and Studio theme. Use `--studio-*` theme variables. HTML/body have zero margins; include spacing inside the viewport. Scripts, external resources, navigation, and uploads remain disabled.

Declare callbacks as above. Use `data-callback="id"` on a form with named inputs or a standalone `type="button"` button. The host handles submission.

Older threads can use `orchestration_send`, `agent_id="workspace"`, and JSON text: `{"tool":"orchestration_panel","arguments":{"action":"catalog"}}`. Substitute the complete panel action for updates. If the installed server lacks the requested mode, report that limit.
