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
Studio reads the file while the chat is visible. Put the current status first.
The collapsed budget uses 28% of viewport height, between 100px and 280px.
Longer content stays visible with clipping and a fade. The Expand control opens
a scroll area bounded by half the viewport. Collapse restores the top preview.
The PROGRESS.md link opens the complete original file.

Studio measures the complete rendered revision at the current width and font.
It writes measurements to `PROGRESS.layout.json` beside `PROGRESS.md`.
Read this feedback file. Do not edit it. Reports contain the renderer,
required and available pixel dimensions, overflow pixels, total and fully visible
top-level lines or list items, the last visible line and heading, the revision,
and measurement time. A paragraph or heading counts as one top-level line.
Wrapped lines count only when the complete paragraph or item is visible.
The top-level SHA-256 identifies the file content. Reports from separate clients
remain separate. A desktop report cannot erase a recent phone report.
Reports expire after 90 seconds. A previous revision does not validate a new edit.
Expansion does not change the report of the collapsed preview.

After an edit, run the check command in your runtime instructions once.
It reads the feedback file and can wait up to three seconds for a visible client.
Its hint describes the client with the fewest fully visible lines or items.
Overflow is allowed. Trim only when important lines are hidden.
Do not repeat rewrites just to remove overflow. The command returns:

- Exit 0: the current revision is visible, including clipped content, or the file is empty.
- Exit 1: a current client reports unsupported content, or a file-read error occurs.
- Exit 2: the current revision has no recent measurement.

Older clients that hide overflow still report failure until they update.
If no client is visible, leave a short status and treat the layout as unmeasured.
Do not start a browser, wake another agent, or repeat checks indefinitely.

An empty or missing file clears the display. A read error, invalid UTF-8, or an
oversized file shows an error. The source stays unchanged. Studio accepts an atomic
file replacement and checks the next revision. The file does not run scripts,
connect command output, or schedule model turns. Put requests that need a user
response in the conversation or user-task flow.
