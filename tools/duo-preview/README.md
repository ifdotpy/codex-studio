# Duo :8444 preview (work in progress, not for main)

Base: release ce962724d (server API schema df5e0890…). On top, local commits:
#528 port (18 commits after ddc5afb30), #532 port (to 44dfe2610), #533 port
(to 95c95d31b) and the #533 recovery delta (136b320c8).

Build: `cd workspaces/client/apps/web && pnpm build:wasm && npx vite build --outDir /tmp/duo-ios-preview/dist --emptyOutDir`,
then copy `duo-log.js` into the dist and add `<script src="./duo-log.js">` to index.html.

Serve: `NODE_OPTIONS=--max-http-header-size=65536 npx vite preview --config tools/duo-preview/vite.preview.config.mjs --outDir /tmp/duo-ios-preview/dist`
on 127.0.0.1:8793; `tailscale serve` forwards :8444 to it. The device logger writes /tmp/duo-ios-preview/device-log.jsonl (200 MB cap).

Rollback: /tmp/duo-ios-preview/dist-prev (previous build). Swap the directories and restart the preview (sirv caches the file list).

Checks: `pw/pane-shots.mjs <out> <url>` (open-team, portrait-sidebar, closed-team with the native viewport modeled),
`pw/update-screen.mjs <png> <url>` (forced schema mismatch), `pw/err.mjs <url>` (error box).
