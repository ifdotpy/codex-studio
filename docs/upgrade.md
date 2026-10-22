# Upgrade an existing Studio installation

Keep the existing state-directory identity. Never start a second backend against
an occupied state directory or stop active agents, monitors, terminals, or waves
for a source update. Record the current commit and diagnostics before delivery.

The removed Python preflight, payload-migration, and supervisor scripts are no
longer supported commands. Their historical output does not verify the Rust server.
Do not run an old migration script against current state.

1. Prepare the reviewed source in an isolated checkout and install locked tools
   with `pnpm install --frozen-lockfile`. Preserve unrelated local changes.
2. Run `just test-fast` before review and `just test-comprehensive` on the exact
   publication commit. See [testing](testing.md) for evidence boundaries.
3. On this Linux host, use only `just deliver-server <commit>` for the live server.
   It owns release snapshots, the current link, service restart, health checks,
   and rollback. Never edit systemd drop-ins or the binary path by hand.
4. Confirm the resulting build identity, preserved state-directory path, and
   health through diagnostics. A failed or unknown check remains a failed upgrade.

For platform-specific installation, see [the product guide](product-guide.md),
[the CLI guide](cli.md), and [relocation](relocating-installation.md).
A desktop package requires the comprehensive gate on its exact source commit;
a fixture pass alone does not verify an installed build.
