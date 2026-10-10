# Codex Studio

Codex Studio is a desktop workspace for a lead agent and many workers. It uses
Codex app-server for model sessions, tools, and permissions, and adds durable
orchestration, agent messages, command monitors, and user replies.

## Install and run

Requirements and detailed setup are in the [product guide](docs/product-guide.md).

```sh
pnpm install --frozen-lockfile
git config --local core.hooksPath .githooks
python3 workspaces/runtime/apps/server/src/install-cli.py
pnpm --filter codex-agents-web run build
pnpm --filter codex-agents-desktop run start
```

The root [`package.json`](package.json) owns root commands. Each app manifest
owns its package commands; see the workspace guides below.

## Where things live

- [Runtime](workspaces/runtime/README.md): Python server, VM guest, and prompts.
- [Providers](workspaces/providers/README.md): provider process integrations.
- [Client](workspaces/client/README.md): web renderer and Electron desktop app.
- [Tooling](workspaces/tooling/README.md): repository checks and commit hooks.
- [Documentation](docs): product, operator, design, testing, and verification pages.
- [Move an existing installation](docs/relocating-installation.md) while
  preserving its state directory and active work.
- [Workspace migration specification](docs/plans/rust-workspaces.md) and
  [tracking issue #37](https://github.com/ifdotpy/codex-studio/issues/37).

For test ownership and suite selection, see [docs/testing.md](docs/testing.md).
