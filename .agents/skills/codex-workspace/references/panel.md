# Codex Studio panel

These rules apply to the persistent agent panel in Codex Studio. They do not apply to native Codex CLI sessions.

## Purpose and layout

Use `orchestration_panel` to show a visual state, result, or useful control between the chat and composer. Each agent owns one panel. Prefer a stage track, segmented bar, compact diagram, measured counters, or a small card composition. Shape, size, and position must communicate information. A paragraph or a row of labels with arrows is not a progress display.

Use short labels and explicit states such as running, waiting, failed, and complete. Distinguish a finished command from an accepted result. Show counts or percentages only when measurements support them. Use labels with color. Keep detailed evidence in chat or linked workspace files. Update the panel when its information changes. Do not poll or send repeated messages to refresh it.

The content viewport is exactly 150 CSS pixels high and fills the available width. `html` and `body` fill that viewport with zero margins. Include all padding and borders inside the 150 pixels with `box-sizing:border-box`. Put spacing inside the composition. Adapt the layout to widths of 320, 640, and 1000 pixels.

The canvas always matches the chat background. A single full-viewport `div` or `main` wrapper becomes transparent. Style nested cards when useful. Use Studio typography and variables: `--studio-surface`, `--studio-text`, `--studio-muted`, `--studio-accent`, `--studio-border`, `--studio-radius`, and `--studio-font`. The theme supplies controls, not a layout.

## Publish and inspect

Use `action=set` with `html`, optional `css`, and declared `callbacks`. The server measures the layout at all three widths before saving it. It returns a PNG at 1000×150 pixels. Inspect the image for readability, contrast, visual structure, and fit.

Everything must fit, including forms, controls, hover states, and open details. Do not use scrolling, clipping, or ellipsis to conceal overflow. Simplify or rearrange the content. If measurement or rendering fails, the previous content and callbacks stay unchanged. Fix the reported problem and submit a new tool call. `action=get` can inspect a legacy panel; `action=clear` removes the content.

## Controls and events

Declare each callback with `id`, `label`, and `fields`, the exact input names. Use `<form data-callback="review">` with named inputs and a submit button. For a standalone button, use `type="button"`, `data-callback="refresh"`, and an empty `fields` list.

A `panel_callback` event contains `callback`, `label`, `panelVersion`, and `values`, whose values are arrays of strings. It wakes the agent after a final answer. Handle the action and submitted values. Each callback accepts one submission per panel version. Publish an updated panel to acknowledge it and enable another submission when needed. An interaction does not authorize unrelated actions.

Scripts, external resources, navigation, and file uploads remain disabled. The host handles callbacks. For older managed threads, call `orchestration_send` with `agent_id="workspace"` and JSON in `text`: `{"tool":"orchestration_panel","arguments":{"action":"get"}}`. For updates, replace `arguments` with the complete panel action.

Adapt [the progress example](../assets/panel-progress.json) when useful. Replace its sample counts and states with verified task data.
