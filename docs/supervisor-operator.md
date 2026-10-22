# Process supervisor operator notes

The Rust process supervisor owns provider child processes and durable receipts.
Its executable is `codex-process-supervisor`; the installed server/recovery path
starts it for the configured state directory. Preserve that directory's identity.

The removed Python supervisor's status and per-handle administration CLI are
unavailable. There is no compatibility alias. Do not translate a stale command
into a broad PID kill or replace a live supervisor to remove one unknown handle.
Confirm process identity and ownership through current diagnostics first.

Leave active account children, commands, monitors, and terminal sessions running.
For service installation and recovery, see [the CLI guide](cli.md),
[restart recovery](restart-recovery.md), and [Linux setup](linux-server-setup.md).
Use `just test studio-process-supervisor` for isolated supervisor tests.
