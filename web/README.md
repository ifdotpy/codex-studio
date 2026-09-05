# Codex agents web client

React and TypeScript components, built with Vite. Mantine provides controls,
menus, dialogs, drawers, and the shared theme. Lucide provides icons. The Python server owns agents,
SQLite, message delivery, command monitors, and the complaint book.

## Run

From this directory:

```bash
npm ci
npm run build
../scripts/codex-canvas
```

Open <http://127.0.0.1:4620>. The server serves `dist/`.
Build output is local and ignored by Git. Rebuild it after a frontend change.
The server reports a missing build instead of serving an older client.

## Develop

Keep the Python server on port 4620. Start Vite in another terminal:

```bash
npm run dev
```

Vite proxies `/api` to the local Python server. `npm run build` checks TypeScript
and creates the production bundle. `npm test` builds that bundle and checks it in
a separate headless Chrome process against an isolated server and database.
Set `CHROME_BIN` if Chrome uses another executable path.

## Source

- `src/App.tsx`: screen selection, drafts, conversation actions, imports, and team panel.
- `src/components/Sidebar.tsx`: lead and agent tabs, search, previews, unread state, inline names, and deletion.
- `src/components/Conversation.tsx`: Markdown messages, agent bubbles, composer, and questions.
- `src/components/Canvas.tsx`: the shared agent graph and stored positions.
- `src/components/ComplaintBook.tsx`: complaints, lead decisions, and user submissions.
- `src/components/Usage.tsx`: context usage, compaction count, and account limits.
- `src/hooks.ts`: server snapshots and transcript updates.

The sidebar renders 60 rows initially and adds rows as the user scrolls.
Unread markers and drafts remain in browser storage. Chat names remain in SQLite.
Native tools and permissions remain in the Codex app-server.

## UI conventions

Use `src/theme.ts` for control defaults and theme tokens. Use Mantine components
for forms and overlays. Keep layout rules in `src/style.css`.

The main conversation uses an AI chat layout with unboxed assistant responses.
Only agent rooms use messenger avatars, timestamps, unread markers, and paired
message bubbles. The team panel and navigation become drawers on narrow screens.

New transcript records preserve input sources. The client presents orchestration
events as expandable activity, separate from user messages. Older records retain
their original display when source metadata is absent.

The browser checks use 40 workers and more than 60 rooms. They cover widths from
320 to 1920 pixels, long drafts, table overflow, focus return, and menu placement.
Screenshots use an isolated database. They do not represent live model results.
