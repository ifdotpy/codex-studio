# Upgrade an existing Studio installation

Keep the existing state-directory identity. Never start a second backend against
an occupied state directory or stop active agents, monitors, terminals, or waves
for a source update. Record the current commit and diagnostics before delivery.

The removed Python preflight, payload-migration, and supervisor scripts are no
longer supported commands. Their historical output does not verify the Rust server.
Do not run an old migration script against current state.

The desktop no longer imports legacy browser profile archives or old native
preference rows. A profile that was never imported needs fresh server pairing;
its archived files remain on disk.

[Runtime migrations](runtime-migrations.md) use per-step rollback: a failed
transaction rolls back, completed steps remain, and startup resumes safely from
retained state. The server must not serve requests before the migration barrier
succeeds. Release rollback restores executable artifacts, not the databases or
registry. An older binary must support the resulting format or refuse to open it.

1. Prepare the reviewed source in an isolated checkout and install locked tools
   with `pnpm install --frozen-lockfile`. Preserve unrelated local changes.
2. Run `just test-fast` before review and `just test-comprehensive` on the exact
   publication commit. See [testing](testing.md) for evidence boundaries.
3. On this Linux host, use only `just deliver-server <commit>` for the live server.
   It owns release snapshots, the current link, service restart, health checks,
   and stopping an unhealthy release for operator recovery. Never edit systemd drop-ins or the binary path by hand.
4. Confirm the resulting build identity, preserved state-directory path, and
   health through diagnostics. A failed or unknown check remains a failed upgrade.

For platform-specific installation, see [the product guide](product-guide.md),
[the CLI guide](cli.md), and [relocation](relocating-installation.md).
A desktop package requires the comprehensive gate on its exact source commit;
a fixture pass alone does not verify an installed build.
