# Codex Studio progress file

Studio displays each managed agent's `PROGRESS.md` above that chat's composer.
Use ordinary file tools to read and edit it. No panel tool is required.

## File identity

Use the exact path in your runtime instructions or `orchestration_context topic=panel`.
The path has this form:

```text
<stateDir>/progress/<agentId>/PROGRESS.md
```

Studio supplies the actual state directory and agent identity. Do not invent them.
The file stays outside Git and the project's working directory. Agents with the
same working directory have separate files. Do not edit another agent's file.
A project may have its own `PROGRESS.md`; it does not control this display.

The file persists across server restarts and chat switches. Studio creates an
empty file only when it is absent. It does not overwrite existing content or
copy old panel data into the file. Existing read-only permissions still apply.

## Content and updates

Write plain UTF-8 Markdown with at most 128 KiB of content. Use short status text,
verified results, and current blockers. Distinguish a command's successful exit
from acceptance of its result. Do not invent counts or percentages.

Read the current file before an edit. Update it when the facts change.
Studio reads the file in the background while its display is visible. An update
does not require a chat message, command output, or model wake.
The display has a maximum height of 150px. Longer content scrolls within it.

An empty or missing file clears the display. A read error, invalid UTF-8, or an
oversized file shows an error. The interface retains its last valid content until
a later successful read. Studio does not truncate and present an invalid file as
complete. It accepts an atomic file replacement and reads the next revision.

The display uses the same Markdown format and isolated static previews as the chat.
It does not execute scripts or connect command output. It does not provide agent
callbacks. Put requests that need a user response in the existing conversation
or user-task flow.

## Compatibility

`orchestration_panel` and `orchestration_panel_feed` are retired. New calls return
the progress-file instructions. The workspace fallback does not restore them.
Stored legacy panels, callback receipts, and feed records remain intact.
Already accepted work can finish. The change does not cancel active commands,
replace old data, or schedule a model turn.
