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

Write plain UTF-8 Markdown with at most 128 KiB of content. This is a file-read
limit, not a display budget. Keep only the current status, a verified result,
a next step or a blocker. Put details in another file or the conversation.
Do not invent counts or percentages.

Use passive Markdown: paragraphs, headings, lists, emphasis, inline code and links.
Do not use images, tables, fenced code or raw HTML. The status panel does not
provide interactive previews or hidden sections. Links can open a complete file.
Relative links use the progress file directory. Use absolute paths for project files.

Read the current file before an edit. Update it when the facts change.
Studio reads the file while the chat is visible. The panel has no scrollbar.
Its maximum height is 150px, with less space in a small window.
Studio measures the complete rendered revision at the current width and font.
If it does not fit, Studio shows a short notice with access to the original file.
It does not cut text, shrink the font, or show old content as current.

Studio writes measurements to `PROGRESS.layout.json` beside `PROGRESS.md`.
Read this feedback file. Do not edit it. Each report contains the renderer,
required and available pixel dimensions, file revision and measurement time.
The top-level SHA-256 identifies the file content. Reports from separate clients
remain separate. A wide desktop success cannot erase a recent narrow-client failure.
Reports expire after 90 seconds. A previous revision does not validate a new edit.

After an edit, run the check command in your runtime instructions. It reads the
feedback file and can wait up to three seconds for a visible client.
Shorten or simplify the status until the check succeeds for the current revision.
Use the measured dimensions. Do not estimate fit from line or character counts.
The command returns:

- Exit 0: the current revision fits recent visible clients, or the file is empty.
- Exit 1: a current client reports overflow, unsupported content, or a file-read error.
- Exit 2: the current revision has no recent measurement.

If no client is visible, leave a short status and treat fit as unmeasured.
Do not start a browser, wake another agent, or repeat checks indefinitely.
A later narrower window or a larger font can require a shorter status.
The interface checks that window again before showing the full revision.

An empty or missing file clears the display. A read error, invalid UTF-8, or an
oversized file shows an error. The source stays unchanged. Studio accepts an atomic
file replacement and checks the next revision. The file does not run scripts,
connect command output, or schedule model turns. Put requests that need a user
response in the conversation or user-task flow.

## Compatibility

`orchestration_panel` and `orchestration_panel_feed` are retired. New calls return
the progress-file instructions. The workspace fallback does not restore them.
Stored legacy panels, callback receipts, and feed records remain intact.
Already accepted work can finish. The change does not cancel active commands,
replace old data, or schedule a model turn.
