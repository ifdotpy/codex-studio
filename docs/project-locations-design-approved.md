# Approved design: project locations and sidebar status line (user approval 2026-10-09 09:16 UTC)

Model: a project is not bound to one server. A chat is bound to one server.

- A project has locations: one folder per server.
- Existing projects keep their current folder as the location on this server.
- Add project may create a project that has only a remote location (needs a stable abstract project ID; keep path IDs for existing projects).

Add project dialog: Server selector (This Mac default, paired servers, offline servers grey and disabled), folder browse on that server, Add.

Project settings > Folders:

- One row per location: server, folder, git short HEAD, ••• (Remove).
- "Add folder": Server selector + folder browse on that server. A folder with the same git origin is highlighted as a suggestion ("same git origin"). Never added automatically.

New chat in a project: server cards (server name, Active/Offline, folder). Default: the server of the last chat. A server without a folder shows "+ Add folder on this server". The chat runs on the chosen server.

Sidebar chat rows (match the current style):

- Line 1: chat name (as today). Status indicator on the right exactly as today (ChatStatus: dot, spinner, warning).
- Line 2 (status line, muted, 11px): ProviderMark (the existing monochrome mark, about 11px) and the server alias. No model name.
- Server alias: at most 3 letters, uppercase. Defaults: MAC (this Mac), MBP (igor-mbp), WSL (kukuka-win WSL server), WIN (kukuka-win native server). Editable in Settings > Servers card. Derive defaults for new servers; keep them unique.
- Project name row: no server badges.

Compact project:

- Compact is the default state for every project (existing explicit user choices stay).
- At the bottom of a compact project: "Show more (N)". When expanded: "Show less". Remove "Show all".
