# Server updates

The removed Python backend's in-process patch publisher is no longer available.
Do not publish its temporary patch files or live-update manifests to the Rust server.

Use `just deliver-server <commit>` to update the live server on this Linux host.
That recipe owns release snapshots, the current link, service restart, health
verification, rollback, and pruning. The entire delivery runs in a separate
transient systemd user service and survives the restart of its calling backend.
The printed journal command retrieves progress and the final exit code after a
caller disconnect; a surviving caller receives logs and the exit status. See
[Linux setup](linux-server-setup.md) for the handoff and retry rules. Preserve the existing state-directory identity and
active work. Never edit service drop-ins or binary paths by hand.

For installation and update commands, see [the CLI guide](cli.md) and
[the product guide](product-guide.md). Historical patch evidence describes only
the revision where it was collected.
